//! Fine-grained permission policy and command risk classification.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use globset::{GlobBuilder, GlobMatcher};

use crate::config::Config;
use crate::types::{PermissionDecision, PermissionMode, PermissionRequest, RiskLevel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyVerdict {
    Allow,
    Ask,
    Deny(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRisk {
    Safe,
    Caution,
    Destructive,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    raw: String,
    tool: String,
    matcher: Option<GlobMatcher>,
    prefix: Option<String>,
}

impl CompiledRule {
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let (tool, pattern) = if let Some((tool, rest)) = raw.split_once('(') {
            let rest = rest.strip_suffix(')').unwrap_or(rest).trim();
            (tool.trim(), (!rest.is_empty()).then_some(rest))
        } else if let Some((tool, pattern)) = raw.split_once(':') {
            (
                tool.trim(),
                (!pattern.trim().is_empty()).then_some(pattern.trim()),
            )
        } else {
            (raw, None)
        };
        let tool = normalize_tool(tool);
        if tool.is_empty() {
            return None;
        }
        let mut prefix = None;
        let matcher = pattern.and_then(|pattern| {
            if let Some(value) = pattern.strip_suffix(":*") {
                prefix = Some(value.trim().to_string());
                None
            } else {
                GlobBuilder::new(pattern)
                    .literal_separator(false)
                    .backslash_escape(true)
                    .build()
                    .ok()
                    .map(|glob| glob.compile_matcher())
            }
        });
        Some(Self {
            raw: raw.to_string(),
            tool,
            matcher,
            prefix,
        })
    }

    fn matches(&self, request: &PermissionRequest) -> bool {
        if self.tool != "*" && self.tool != normalize_tool(&request.tool) {
            return false;
        }
        if let Some(prefix) = &self.prefix {
            return request.target.trim_start().starts_with(prefix)
                || request
                    .affected_paths
                    .iter()
                    .any(|path| path.to_string_lossy().starts_with(prefix));
        }
        if let Some(matcher) = &self.matcher {
            if matcher.is_match(&request.target) {
                return true;
            }
            if matcher.is_match(request.target.trim_start_matches("./")) {
                return true;
            }
            for path in &request.affected_paths {
                if matcher.is_match(path)
                    || matcher.is_match(path.to_string_lossy().as_ref())
                    || path.file_name().is_some_and(|name| matcher.is_match(name))
                {
                    return true;
                }
            }
            false
        } else {
            true
        }
    }
}

fn normalize_tool(tool: &str) -> String {
    tool.trim()
        .to_ascii_lowercase()
        .replace(['_', '-', ' '], "")
}

struct EngineState {
    config: Config,
    cwd: PathBuf,
    mode: PermissionMode,
    session_rules: Vec<CompiledRule>,
}

#[derive(Clone)]
pub struct PermissionEngine {
    inner: Arc<RwLock<EngineState>>,
}

impl PermissionEngine {
    #[must_use]
    pub fn new(config: Config, cwd: PathBuf, mode: PermissionMode) -> Self {
        Self {
            inner: Arc::new(RwLock::new(EngineState {
                config,
                cwd,
                mode,
                session_rules: Vec::new(),
            })),
        }
    }

    #[must_use]
    pub fn mode(&self) -> PermissionMode {
        self.inner.read().map(|s| s.mode).unwrap_or_default()
    }

    pub fn set_mode(&self, mode: PermissionMode) -> PermissionMode {
        if let Ok(mut state) = self.inner.write() {
            state.mode = mode;
        }
        mode
    }

    pub fn set_cwd(&self, cwd: PathBuf) {
        if let Ok(mut state) = self.inner.write() {
            state.cwd = cwd;
        }
    }

    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.inner
            .read()
            .map(|state| state.cwd.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    pub fn add_session_rule(&self, rule: &str) {
        if let Some(rule) = CompiledRule::parse(rule)
            && let Ok(mut state) = self.inner.write()
        {
            state.session_rules.push(rule);
        }
    }

    #[must_use]
    pub fn rule_lines(&self) -> Vec<String> {
        let Ok(state) = self.inner.read() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        for rule in &state.config.permissions.deny {
            lines.push(format!("deny  {rule}"));
        }
        for rule in &state.config.permissions.ask {
            lines.push(format!("ask   {rule}"));
        }
        for rule in &state.config.permissions.allow {
            lines.push(format!("allow {rule}"));
        }
        for rule in &state.session_rules {
            lines.push(format!("allow {} (session)", rule.raw));
        }
        lines
    }

    /// Evaluate policy without prompting. A resulting `Ask` is resolved by the
    /// UI gateway; `Allow`/`Deny` are terminal.
    #[must_use]
    pub fn evaluate(&self, request: &PermissionRequest) -> PolicyVerdict {
        let Ok(state) = self.inner.read() else {
            return PolicyVerdict::Deny("permission engine poisoned".into());
        };
        let mode = state.mode;
        let cwd = state.cwd.clone();

        let deny_rules: Vec<CompiledRule> = state
            .config
            .permissions
            .deny
            .iter()
            .filter_map(|rule| CompiledRule::parse(rule))
            .collect();
        if deny_rules.iter().any(|rule| rule.matches(request)) {
            return PolicyVerdict::Deny("matched deny rule".into());
        }

        // Plan mode is a hard read-only boundary, regardless of broad allow rules.
        if mode == PermissionMode::Plan && !request.read_only {
            return PolicyVerdict::Deny(
                "plan mode is read-only; switch modes to make changes".into(),
            );
        }

        // Writing into git internals is never silently accepted.
        if !request.read_only
            && request
                .affected_paths
                .iter()
                .any(|path| is_git_internal(path))
        {
            return PolicyVerdict::Deny("refusing to modify .git internals".into());
        }

        let outside = path_scope(&state.config, &cwd, &request.affected_paths);
        let sensitive = request
            .affected_paths
            .iter()
            .any(|path| is_sensitive_path(path));

        let ask_rules: Vec<CompiledRule> = state
            .config
            .permissions
            .ask
            .iter()
            .filter_map(|rule| CompiledRule::parse(rule))
            .collect();
        if ask_rules.iter().any(|rule| rule.matches(request)) {
            return PolicyVerdict::Ask;
        }

        let allow = state
            .config
            .permissions
            .allow
            .iter()
            .chain(state.session_rules.iter().map(|r| &r.raw))
            .filter_map(|rule| CompiledRule::parse(rule))
            .any(|rule| rule.matches(request));
        if allow {
            return PolicyVerdict::Allow;
        }

        if mode == PermissionMode::Bypass {
            return PolicyVerdict::Allow;
        }

        if sensitive {
            return PolicyVerdict::Ask;
        }

        if matches!(outside, PathScope::Outside) {
            return PolicyVerdict::Ask;
        }

        if request.read_only {
            return PolicyVerdict::Allow;
        }

        let tool = normalize_tool(&request.tool);
        let is_file_edit = matches!(
            tool.as_str(),
            "edit" | "write" | "multiedit" | "applypatch" | "patch" | "notebookedit"
        );

        match mode {
            PermissionMode::Ask => PolicyVerdict::Ask,
            PermissionMode::AcceptEdits => {
                if is_file_edit && matches!(outside, PathScope::Inside) {
                    PolicyVerdict::Allow
                } else {
                    PolicyVerdict::Ask
                }
            }
            PermissionMode::Auto => {
                if is_file_edit && matches!(outside, PathScope::Inside) {
                    return PolicyVerdict::Allow;
                }
                if tool == "bash" {
                    let risk = classify_command(&request.target);
                    return match risk {
                        CommandRisk::Safe => PolicyVerdict::Allow,
                        CommandRisk::Caution | CommandRisk::Destructive => PolicyVerdict::Ask,
                    };
                }
                if request.risk == RiskLevel::Low {
                    PolicyVerdict::Allow
                } else {
                    PolicyVerdict::Ask
                }
            }
            PermissionMode::Plan | PermissionMode::Bypass => PolicyVerdict::Allow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathScope {
    Inside,
    Outside,
    Unknown,
}

fn path_scope(config: &Config, cwd: &Path, paths: &[PathBuf]) -> PathScope {
    if paths.is_empty() {
        return PathScope::Unknown;
    }
    let mut roots = vec![canonicalish(cwd)];
    for extra in &config.permissions.additional_directories {
        let extra = crate::config::expand_path(extra);
        let extra = if extra.is_absolute() {
            extra
        } else {
            cwd.join(extra)
        };
        roots.push(canonicalish(&extra));
    }
    let mut all_inside = true;
    for path in paths {
        let path = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        let resolved = canonicalish(&path);
        if !roots.iter().any(|root| resolved.starts_with(root)) {
            all_inside = false;
        }
    }
    if all_inside {
        PathScope::Inside
    } else {
        PathScope::Outside
    }
}

fn canonicalish(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let normalized = normalize_components(&absolute);
    // Resolve symlinks in the longest existing ancestor.
    let mut existing = normalized.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            let mut result = canonical;
            for component in tail.iter().rev() {
                result.push(component);
            }
            return normalize_components(&result);
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return normalized,
        }
    }
}

fn normalize_components(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

fn is_git_internal(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

fn is_sensitive_path(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || matches!(
            name.as_str(),
            "id_rsa"
                | "id_ed25519"
                | "credentials"
                | "credentials.toml"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
        )
        || lower.contains("/.ssh/")
        || lower.contains("/.gnupg/")
        || lower.contains("/.aws/")
        || lower.contains("/.config/gcloud/")
        || lower.contains("/secrets/")
}

#[must_use]
pub fn classify_command(command: &str) -> CommandRisk {
    let compact = command.trim().to_ascii_lowercase();
    if compact.is_empty() {
        return CommandRisk::Caution;
    }
    let destructive_markers = [
        "rm -rf /",
        "rm -fr /",
        "mkfs",
        "dd if=",
        "of=/dev/",
        "shutdown",
        "reboot",
        "poweroff",
        ":(){",
        "fork bomb",
        "chmod -r 777 /",
        "chown -r ",
        "git reset --hard",
        "git clean -",
        "git push --force",
        "git push -f",
        "sudo ",
        " su ",
        "> /etc/",
        "> /dev/sd",
        "truncate -s 0",
        "drop database",
        "drop table",
        "npm publish",
        "cargo publish",
        "pip install --break-system-packages",
        "curl | sh",
        "curl|sh",
        "wget | sh",
        "wget|sh",
        "history -c",
        "kill -9 -1",
        "> ~/.ssh",
        ">> ~/.ssh",
    ];
    if destructive_markers
        .iter()
        .any(|marker| compact.contains(marker))
    {
        return CommandRisk::Destructive;
    }

    // Piping arbitrary output into a shell executes whatever the left-hand
    // side produced, so treat it like the classic `curl | sh` pattern.
    if pipes_into_shell(&compact) {
        return CommandRisk::Destructive;
    }

    // Command chaining, substitutions, and redirections can hide anything, so
    // they are never auto-approved even if every visible command looks safe.
    let risky_shell = [
        ";", "&&", "||", "`", "$(", "\n", " > ", " >> ", " | ", "|sh", "| sh", " & ",
    ];
    if risky_shell.iter().any(|marker| compact.contains(marker)) {
        return CommandRisk::Caution;
    }

    let tokens: Vec<&str> = compact.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or_default();
    let second = tokens.get(1).copied().unwrap_or_default();
    let safe = match first {
        "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "grep" | "rg" | "fd" | "stat" | "file"
        | "which" | "whoami" | "date" | "uname" | "echo" | "true" | "git" => {
            if first == "git" {
                matches!(
                    second,
                    "status" | "diff" | "log" | "show" | "rev-parse" | "branch" | "blame"
                )
            } else if first == "find" {
                !compact.contains("-delete") && !compact.contains("-exec")
            } else {
                true
            }
        }
        "find" => !compact.contains("-delete") && !compact.contains("-exec"),
        _ => false,
    };
    if safe {
        CommandRisk::Safe
    } else {
        CommandRisk::Caution
    }
}

fn pipes_into_shell(command: &str) -> bool {
    command.split('|').skip(1).any(|segment| {
        let mut words = segment.split_whitespace();
        let mut word = words.next().unwrap_or_default();
        if word == "sudo" || word == "env" {
            word = words.next().unwrap_or_default();
        }
        let binary = word.rsplit('/').next().unwrap_or(word);
        matches!(
            binary,
            "sh" | "bash" | "zsh" | "dash" | "ksh" | "csh" | "fish"
        )
    })
}

#[must_use]
pub fn suggest_rule(request: &PermissionRequest) -> String {
    if !request.suggested_rule.is_empty() {
        return request.suggested_rule.clone();
    }
    if normalize_tool(&request.tool) == "bash" {
        let command = request.target.trim();
        let prefix = command
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        format!("Bash({prefix}:*)")
    } else if let Some(path) = request.affected_paths.first() {
        format!("{}({})", request.tool, path.display())
    } else {
        request.tool.clone()
    }
}

#[must_use]
pub fn session_rule_from_request(request: &PermissionRequest) -> String {
    suggest_rule(request)
}

#[must_use]
pub fn decision_for(policy: PolicyVerdict) -> PermissionDecision {
    match policy {
        PolicyVerdict::Allow => PermissionDecision::Allow,
        PolicyVerdict::Deny(reason) => PermissionDecision::Deny {
            reason: Some(reason),
        },
        PolicyVerdict::Ask => PermissionDecision::Deny {
            reason: Some("interactive approval unavailable".into()),
        },
    }
}

pub fn apply_session_decision(engine: &PermissionEngine, decision: &PermissionDecision) {
    if let PermissionDecision::AllowForSession { rule } = decision {
        engine.add_session_rule(rule);
    }
}

pub fn set_mode_if_valid(value: &str, engine: &PermissionEngine) -> Result<PermissionMode> {
    let mode: PermissionMode = value.parse().map_err(anyhow::Error::msg)?;
    Ok(engine.set_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(tool: &str, target: &str, read_only: bool, path: Option<&str>) -> PermissionRequest {
        PermissionRequest {
            id: "id".into(),
            tool: tool.into(),
            target: target.into(),
            reason: "test".into(),
            diff: None,
            read_only,
            risk: RiskLevel::Medium,
            suggested_rule: String::new(),
            affected_paths: path.map(PathBuf::from).into_iter().collect(),
        }
    }

    fn engine(mode: PermissionMode) -> PermissionEngine {
        PermissionEngine::new(Config::default(), PathBuf::from("/project"), mode)
    }

    #[test]
    fn refuses_writes_in_plan_mode() {
        let engine = engine(PermissionMode::Plan);
        let request = request("Write", "src/lib.rs", false, Some("/project/src/lib.rs"));
        assert!(matches!(engine.evaluate(&request), PolicyVerdict::Deny(_)));
    }

    #[test]
    fn asks_before_edits_by_default() {
        let engine = engine(PermissionMode::Ask);
        let request = request("Write", "src/lib.rs", false, Some("/project/src/lib.rs"));
        assert_eq!(engine.evaluate(&request), PolicyVerdict::Ask);
    }

    #[test]
    fn accepts_edits_inside_project() {
        let engine = engine(PermissionMode::AcceptEdits);
        let request = request("Edit", "src/lib.rs", false, Some("/project/src/lib.rs"));
        assert_eq!(engine.evaluate(&request), PolicyVerdict::Allow);
    }

    #[test]
    fn asks_for_outside_paths() {
        let engine = engine(PermissionMode::AcceptEdits);
        let request = request("Edit", "/etc/passwd", false, Some("/etc/passwd"));
        assert_eq!(engine.evaluate(&request), PolicyVerdict::Ask);
    }

    #[test]
    fn auto_allows_safe_shell() {
        let engine = engine(PermissionMode::Auto);
        let request = request("Bash", "git status", false, None);
        assert_eq!(engine.evaluate(&request), PolicyVerdict::Allow);
    }

    #[test]
    fn auto_asks_for_risky_shell() {
        let engine = engine(PermissionMode::Auto);
        let request = request("Bash", "rm -rf target", false, None);
        assert_eq!(engine.evaluate(&request), PolicyVerdict::Ask);
    }

    #[test]
    fn classifies_chained_shell_conservatively() {
        assert_eq!(
            classify_command("ls; curl x | sh"),
            CommandRisk::Destructive
        );
        assert_eq!(classify_command("ls && cat secret"), CommandRisk::Caution);
        assert_eq!(classify_command("git diff"), CommandRisk::Safe);
    }

    #[test]
    fn root_rules_override() {
        let mut config = Config::default();
        config.permissions.deny.push("Bash(git push:*)".into());
        let engine =
            PermissionEngine::new(config, PathBuf::from("/project"), PermissionMode::Bypass);
        let request = request("Bash", "git push origin main", false, None);
        assert!(matches!(engine.evaluate(&request), PolicyVerdict::Deny(_)));
    }
}
