//! Built-in and user-defined specialized agents.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indexmap::IndexMap;

use crate::frontmatter::{MarkdownDoc, list_field, parse};
use crate::paths::AaykraPaths;
use crate::types::PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSource {
    Builtin,
    Global,
    Project,
}

impl AgentSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub allowed_tools: Option<Vec<String>>,
    pub denied_tools: Vec<String>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub permission_mode: Option<PermissionMode>,
    pub mcp_servers: Option<Vec<String>>,
    pub source: AgentSource,
    pub path: Option<PathBuf>,
}

impl AgentDefinition {
    #[must_use]
    pub fn tool_allowed(&self, tool: &str) -> bool {
        if self
            .denied_tools
            .iter()
            .any(|denied| tool_name_eq(denied, tool))
        {
            return false;
        }
        self.allowed_tools
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|allowed| tool_name_eq(allowed, tool)))
    }
}

fn tool_name_eq(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: IndexMap<String, AgentDefinition>,
}

impl AgentRegistry {
    pub fn discover(project_root: &Path, paths: &AaykraPaths) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut agents = IndexMap::new();
        for definition in builtin_agents() {
            agents.insert(definition.name.clone(), definition);
        }
        for (root, source) in [
            (paths.agents.clone(), AgentSource::Global),
            (
                crate::paths::project_agent_dir(project_root),
                AgentSource::Project,
            ),
        ] {
            for definition in load_directory(&root, source, &mut warnings) {
                if let Some(existing) = agents.get(&definition.name) {
                    warnings.push(format!(
                        "agent `{}` from {} overrides {}",
                        definition.name,
                        definition.source.label(),
                        existing.source.label()
                    ));
                }
                agents.insert(definition.name.clone(), definition);
            }
        }
        agents.sort_keys();
        (Self { agents }, warnings)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name)
    }

    #[must_use]
    pub fn default_agent(&self) -> &AgentDefinition {
        self.agents
            .get("general")
            .or_else(|| self.agents.values().next())
            .expect("built-in general agent must exist")
    }

    #[must_use]
    pub fn all(&self) -> Vec<&AgentDefinition> {
        self.agents.values().collect()
    }

    #[must_use]
    pub fn render_catalog(&self) -> String {
        let mut lines = vec!["Agents:".to_string()];
        for agent in self.agents.values() {
            let mode = agent
                .permission_mode
                .map(PermissionMode::label)
                .unwrap_or("inherit");
            lines.push(format!(
                "  {:<18} {:<52} {:<14} [{}]",
                agent.name,
                truncate(&agent.description, 52),
                mode,
                agent.source.label()
            ));
        }
        lines.join("\n")
    }
}

fn load_directory(
    root: &Path,
    source: AgentSource,
    warnings: &mut Vec<String>,
) -> Vec<AgentDefinition> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut agents = Vec::new();
    for path in paths {
        match load_agent(&path, source) {
            Ok(agent) => agents.push(agent),
            Err(error) => warnings.push(format!("agent {}: {error:#}", path.display())),
        }
    }
    agents
}

fn load_agent(path: &Path, source: AgentSource) -> Result<AgentDefinition> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let MarkdownDoc { frontmatter, body } = parse(&text)?;
    let name = frontmatter
        .get("name")
        .cloned()
        .or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "agent".into());
    let name = sanitize_name(&name);
    if name.is_empty() {
        anyhow::bail!("agent has no valid name");
    }
    let description = frontmatter
        .get("description")
        .cloned()
        .unwrap_or_else(|| body.lines().next().unwrap_or("Custom agent").to_string());
    let allowed = frontmatter
        .get("tools")
        .or_else(|| frontmatter.get("allowed_tools"))
        .map(|v| list_field(v));
    let denied = frontmatter
        .get("disallowed_tools")
        .or_else(|| frontmatter.get("denied_tools"))
        .map(|v| list_field(v))
        .unwrap_or_default();
    let model = frontmatter.get("model").cloned().filter(|v| !v.is_empty());
    let max_turns = frontmatter
        .get("max_turns")
        .and_then(|v| v.parse::<u32>().ok());
    let permission_mode = frontmatter
        .get("permission_mode")
        .and_then(|v| v.parse::<PermissionMode>().ok());
    let mcp_servers = frontmatter.get("mcp_servers").map(|v| list_field(v));
    Ok(AgentDefinition {
        name,
        description,
        system_prompt: body,
        allowed_tools: allowed,
        denied_tools: denied,
        model,
        max_turns,
        permission_mode,
        mcp_servers,
        source,
        path: Some(path.to_path_buf()),
    })
}

fn sanitize_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                Some(ch)
            } else if ch == ' ' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn builtin_agents() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition {
            name: "general".into(),
            description: "Full-capability agent for multi-step implementation work.".into(),
            system_prompt: String::new(),
            allowed_tools: None,
            denied_tools: Vec::new(),
            model: None,
            max_turns: None,
            permission_mode: None,
            mcp_servers: None,
            source: AgentSource::Builtin,
            path: None,
        },
        AgentDefinition {
            name: "explore".into(),
            description: "Fast read-only scout for locating code and answering codebase questions.".into(),
            system_prompt: "You are an exploration specialist. Search thoroughly, read the relevant code, and answer the parent agent's question with concrete file paths and line numbers. You are read-only: do not propose destructive actions. Prefer parallel searches and report only high-signal findings; never invent files or APIs.".into(),
            allowed_tools: Some(
                ["Read", "Glob", "Grep", "RepoMap", "Lsp", "WebFetch", "WebSearch"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            denied_tools: vec!["Write".into(), "Edit".into(), "Bash".into(), "Git".into(), "Agent".into()],
            model: None,
            max_turns: Some(30),
            permission_mode: Some(PermissionMode::Plan),
            mcp_servers: Some(Vec::new()),
            source: AgentSource::Builtin,
            path: None,
        },
        AgentDefinition {
            name: "plan".into(),
            description: "Read-only architect that produces an implementation-ready plan.".into(),
            system_prompt: "You are a software architect. Explore the repository read-only, identify constraints and existing patterns, then produce a precise implementation plan. Include files and line references, sequencing, risks, test strategy, and open questions. Do not modify anything and do not write code in full; plans must be actionable and concise.".into(),
            allowed_tools: Some(
                ["Read", "Glob", "Grep", "RepoMap", "Lsp", "WebFetch", "WebSearch"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            denied_tools: vec!["Write".into(), "Edit".into(), "Bash".into(), "Git".into(), "Agent".into()],
            model: None,
            max_turns: Some(40),
            permission_mode: Some(PermissionMode::Plan),
            mcp_servers: Some(Vec::new()),
            source: AgentSource::Builtin,
            path: None,
        },
        AgentDefinition {
            name: "reviewer".into(),
            description: "Read-only senior code reviewer focused on real defects.".into(),
            system_prompt: "You are a rigorous senior code reviewer. Inspect the requested diff and its surrounding code. Report only concrete, high-confidence findings: correctness, security, data loss, concurrency, compatibility, and missing tests. Order by severity and include file:line references and a minimal fix suggestion. If there are no findings, say so plainly.".into(),
            allowed_tools: Some(
                ["Read", "Glob", "Grep", "RepoMap", "Lsp"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            denied_tools: vec!["Write".into(), "Edit".into(), "Bash".into(), "Agent".into()],
            model: None,
            max_turns: Some(40),
            permission_mode: Some(PermissionMode::Plan),
            mcp_servers: Some(Vec::new()),
            source: AgentSource::Builtin,
            path: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> AaykraPaths {
        let root = tempfile::tempdir().unwrap().keep();
        AaykraPaths {
            sessions: root.join("sessions"),
            checkpoints: root.join("checkpoints"),
            logs: root.join("logs"),
            skills: root.join("skills"),
            agents: root.join("agents"),
            cache: root.join("cache"),
            home: root.clone(),
        }
    }

    #[test]
    fn builtins_are_present() {
        let (registry, warnings) = AgentRegistry::discover(Path::new("/tmp"), &paths());
        assert!(registry.get("explore").is_some());
        assert!(registry.get("general").is_some());
        assert!(warnings.is_empty());
    }

    #[test]
    fn custom_agent_tool_filter() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths();
        std::fs::create_dir_all(&paths.agents).unwrap();
        std::fs::write(
            paths.agents.join("tester.md"),
            "---\nname: tester\ndescription: Tester\ntools: Read, Grep\npermission_mode: plan\nmax_turns: 9\n---\nTest things.",
        )
        .unwrap();
        let (registry, _) = AgentRegistry::discover(root.path(), &paths);
        let tester = registry.get("tester").unwrap();
        assert!(tester.tool_allowed("Read"));
        assert!(!tester.tool_allowed("Write"));
        assert_eq!(tester.max_turns, Some(9));
    }
}
