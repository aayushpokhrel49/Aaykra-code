//! Git workflows as a first-class tool.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{Tool, ToolContext, definition, permission_request, truncate_tool_output};
use crate::types::{RiskLevel, ToolDefinition};

pub struct GitTool;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Status,
    Diff,
    Log,
    Show,
    Blame,
    Add,
    Commit,
    Checkout,
    Branch,
    Push,
    Pull,
    Fetch,
    Pr,
}

#[derive(Debug, Deserialize)]
struct GitInput {
    action: Action,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    amend: bool,
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    stat: bool,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    remote: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    set_upstream: bool,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    draft: bool,
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["status", "diff", "log", "show", "blame", "add", "commit", "checkout", "branch", "push", "pull", "fetch", "pr"],
                "description": "Git operation to perform."
            },
            "message": {"type": "string", "description": "Commit message (commit)."},
            "paths": {"type": "array", "items": {"type": "string"}, "description": "Pathspecs (add, diff, log)."},
            "path": {"type": "string", "description": "Single path (blame, diff, log)."},
            "all": {"type": "boolean", "description": "Stage/commit all tracked changes (add, commit)."},
            "amend": {"type": "boolean", "description": "Amend the previous commit (commit)."},
            "staged": {"type": "boolean", "description": "Show staged diff (diff)."},
            "stat": {"type": "boolean", "description": "Include a diffstat (diff, show)."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Number of log entries. Default 20."},
            "rev": {"type": "string", "description": "Revision for show/blame. Default HEAD."},
            "target": {"type": "string", "description": "Branch or tag to switch to (checkout)."},
            "create": {"type": "boolean", "description": "Create the branch (checkout)."},
            "name": {"type": "string", "description": "Branch name (branch)."},
            "delete": {"type": "boolean", "description": "Delete the branch (branch)."},
            "force": {"type": "boolean", "description": "Force-with-lease push or force branch delete."},
            "remote": {"type": "string", "description": "Remote name. Default origin."},
            "branch": {"type": "string", "description": "Remote branch (push/pull)."},
            "set_upstream": {"type": "boolean", "description": "Set upstream while pushing."},
            "title": {"type": "string", "description": "Pull/merge request title (pr)."},
            "body": {"type": "string", "description": "Pull/merge request body (pr)."},
            "base": {"type": "string", "description": "Base branch for the pull/merge request (pr)."},
            "draft": {"type": "boolean", "description": "Open the pull/merge request as a draft (pr)."}
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

#[async_trait]
impl Tool for GitTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Git",
            "Inspect and change git state without a shell: status, diff, log, show, blame, add, commit, checkout (git switch), branch, push, pull, fetch, and pull/merge requests through gh or glab. Mutations are permission-gated; force operations are high risk.",
            schema(),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let input: GitInput = serde_json::from_value(input).context("invalid Git input")?;
        let (args, read_only, risk, reason) = build_args(&input)?;
        let target = args
            .iter()
            .take(5)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        let mut request = permission_request(
            "Git",
            target,
            reason,
            read_only,
            risk,
            if read_only {
                Vec::new()
            } else {
                vec![context.cwd()]
            },
        );
        request.suggested_rule = format!("Git({}:*)", action_name(&input.action));
        context.authorize(request).await?;
        if read_only {
            run_git(&context.cwd(), &args, Duration::from_secs(60)).await
        } else {
            run_git(&context.cwd(), &args, Duration::from_secs(300)).await
        }
        .map(|output| {
            let output = if output.trim().is_empty() {
                "(no output)".to_string()
            } else {
                output
            };
            truncate_tool_output(context, output)
        })
    }
}

fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Status => "status",
        Action::Diff => "diff",
        Action::Log => "log",
        Action::Show => "show",
        Action::Blame => "blame",
        Action::Add => "add",
        Action::Commit => "commit",
        Action::Checkout => "checkout",
        Action::Branch => "branch",
        Action::Push => "push",
        Action::Pull => "pull",
        Action::Fetch => "fetch",
        Action::Pr => "pr",
    }
}

fn build_args(input: &GitInput) -> Result<(Vec<String>, bool, RiskLevel, &'static str)> {
    let mut args: Vec<String> = Vec::new();
    let mut read_only = false;
    let mut risk = RiskLevel::Medium;
    let reason: &'static str;
    match input.action {
        Action::Status => {
            args.extend(["status".into(), "--short".into(), "--branch".into()]);
            read_only = true;
            risk = RiskLevel::Low;
            reason = "inspect git status";
        }
        Action::Diff => {
            args.push("diff".into());
            if input.staged {
                args.push("--cached".into());
            }
            if input.stat {
                args.push("--stat".into());
            }
            let mut paths = input.paths.clone();
            if let Some(path) = &input.path {
                paths.push(path.clone());
            }
            if !paths.is_empty() {
                args.push("--".into());
                args.extend(paths);
            }
            read_only = true;
            risk = RiskLevel::Low;
            reason = "inspect a diff";
        }
        Action::Log => {
            args.extend([
                "log".into(),
                "--oneline".into(),
                "--decorate".into(),
                "-n".into(),
                input.limit.unwrap_or(20).clamp(1, 200).to_string(),
            ]);
            let mut paths = input.paths.clone();
            if let Some(path) = &input.path {
                paths.push(path.clone());
            }
            if let Some(rev) = &input.rev {
                args.push(rev.clone());
            }
            if !paths.is_empty() {
                args.push("--".into());
                args.extend(paths);
            }
            read_only = true;
            risk = RiskLevel::Low;
            reason = "inspect git history";
        }
        Action::Show => {
            args.extend(["show".into(), "--format=medium".into()]);
            if input.stat {
                args.push("--stat".into());
            }
            args.push(input.rev.clone().unwrap_or_else(|| "HEAD".into()));
            if let Some(path) = &input.path {
                args.push("--".into());
                args.push(path.clone());
            }
            read_only = true;
            risk = RiskLevel::Low;
            reason = "inspect a commit";
        }
        Action::Blame => {
            let path = input.path.as_deref().context("blame requires `path`")?;
            args.extend(["blame".into()]);
            if let Some(rev) = &input.rev {
                args.push(rev.clone());
            }
            args.push("--".into());
            args.push(path.to_string());
            read_only = true;
            risk = RiskLevel::Low;
            reason = "inspect line history";
        }
        Action::Add => {
            args.push("add".into());
            if input.all {
                args.push("-A".into());
            } else {
                if input.paths.is_empty() {
                    bail!("add requires `paths` or all=true");
                }
                args.push("--".into());
                args.extend(input.paths.iter().cloned());
            }
            reason = "stage changes";
        }
        Action::Commit => {
            let message = input
                .message
                .as_deref()
                .context("commit requires `message`")?;
            if message.trim().is_empty() {
                bail!("commit message must not be empty");
            }
            args.push("commit".into());
            if input.all {
                args.push("-a".into());
            }
            if input.amend {
                args.push("--amend".into());
            }
            args.extend(["-m".into(), message.to_string()]);
            if !input.paths.is_empty() {
                args.push("--".into());
                args.extend(input.paths.iter().cloned());
            }
            reason = "create a commit";
        }
        Action::Checkout => {
            let target = input
                .target
                .as_deref()
                .context("checkout requires `target`")?;
            validate_ref(target)?;
            args.push("switch".into());
            if input.create {
                args.push("-c".into());
            }
            args.push(target.to_string());
            reason = "switch branches";
        }
        Action::Branch => {
            if let Some(name) = &input.name {
                validate_ref(name)?;
                args.push("branch".into());
                if input.delete {
                    args.push(if input.force { "-D" } else { "-d" }.into());
                }
                args.push(name.clone());
                reason = if input.delete {
                    "delete a branch"
                } else {
                    "create a branch"
                };
            } else {
                args.extend(["branch".into(), "-vv".into()]);
                read_only = true;
                risk = RiskLevel::Low;
                reason = "list branches";
            }
        }
        Action::Push => {
            let remote = input.remote.as_deref().unwrap_or("origin");
            validate_ref(remote)?;
            args.push("push".into());
            if input.set_upstream {
                args.push("-u".into());
            }
            if input.force {
                args.push("--force-with-lease".into());
            }
            args.push(remote.to_string());
            if let Some(branch) = &input.branch {
                validate_ref(branch)?;
                args.push(branch.clone());
            }
            risk = RiskLevel::High;
            reason = "publish commits to a remote";
        }
        Action::Pull => {
            let remote = input.remote.as_deref().unwrap_or("origin");
            validate_ref(remote)?;
            args.extend(["pull".into(), "--ff-only".into(), remote.to_string()]);
            if let Some(branch) = &input.branch {
                validate_ref(branch)?;
                args.push(branch.clone());
            }
            reason = "fast-forward from a remote";
        }
        Action::Fetch => {
            let remote = input.remote.as_deref().unwrap_or("origin");
            validate_ref(remote)?;
            args.extend(["fetch".into(), "--prune".into(), remote.to_string()]);
            risk = RiskLevel::Low;
            reason = "update remote references";
        }
        Action::Pr => {
            let title = input.title.as_deref().context("pr requires `title`")?;
            if title.trim().is_empty() {
                bail!("pull request title must not be empty");
            }
            let body = input.body.clone().unwrap_or_default();
            if command_exists("gh") {
                args.extend([
                    "pr".into(),
                    "create".into(),
                    "--title".into(),
                    title.to_string(),
                    "--body".into(),
                    body,
                ]);
                if let Some(base) = &input.base {
                    args.extend(["--base".into(), base.clone()]);
                }
                if input.draft {
                    args.push("--draft".into());
                }
                // `gh` is a separate binary; signal it through the command list.
                args.insert(0, "gh".into());
            } else if command_exists("glab") {
                args.extend([
                    "mr".into(),
                    "create".into(),
                    "--title".into(),
                    title.to_string(),
                    "--description".into(),
                    body,
                    "--yes".into(),
                ]);
                if let Some(base) = &input.base {
                    args.extend(["--target-branch".into(), base.clone()]);
                }
                if input.draft {
                    args.push("--draft".into());
                }
                args.insert(0, "glab".into());
            } else {
                bail!(
                    "creating a pull request requires the `gh` (GitHub) or `glab` (GitLab) CLI; install one or open the request in your browser"
                );
            }
            risk = RiskLevel::High;
            reason = "open a pull or merge request";
        }
    }
    Ok((args, read_only, risk, reason))
}

fn validate_ref(reference: &str) -> Result<()> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("git reference must not be empty");
    }
    if reference.starts_with('-') {
        bail!("git reference must not start with `-`");
    }
    if reference.contains("..") || reference.contains('\n') || reference.contains('\0') {
        bail!("invalid git reference `{reference}`");
    }
    if !reference
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./@{}~^+".contains(ch))
    {
        bail!("invalid git reference `{reference}`");
    }
    Ok(())
}

async fn run_git(cwd: &std::path::Path, args: &[String], timeout: Duration) -> Result<String> {
    let program = args.first().map(String::as_str).unwrap_or("git");
    let rest = &args[1..];
    let mut command = if program == "git" {
        let mut command = tokio::process::Command::new("git");
        command.args(rest);
        command
    } else {
        let mut command = tokio::process::Command::new(program);
        command.args(rest);
        command
    };
    let output = tokio::time::timeout(
        timeout,
        command
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_EDITOR", "true")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("git command timed out after {}s", timeout.as_secs()))?
    .with_context(|| format!("could not run {}", args.join(" ")))?;
    let stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&output.stderr)
        .trim_end()
        .to_string();
    if output.status.success() {
        let mut combined = stdout;
        if !stderr.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }
        Ok(combined)
    } else {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        bail!(
            "{} failed (exit {}): {}",
            args.join(" "),
            output.status.code().unwrap_or(1),
            detail.lines().take(40).collect::<Vec<_>>().join("\n")
        )
    }
}

fn command_exists(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate: PathBuf = dir.join(program);
            candidate.is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(action: Action) -> GitInput {
        GitInput {
            action,
            message: None,
            paths: Vec::new(),
            path: None,
            all: false,
            amend: false,
            staged: false,
            stat: false,
            limit: None,
            rev: None,
            target: None,
            create: false,
            name: None,
            delete: false,
            force: false,
            remote: None,
            branch: None,
            set_upstream: false,
            title: None,
            body: None,
            base: None,
            draft: false,
        }
    }

    #[test]
    fn status_is_read_only() {
        let (args, read_only, _, _) = build_args(&input(Action::Status)).unwrap();
        assert_eq!(args[0], "status");
        assert!(read_only);
    }

    #[test]
    fn commit_requires_message() {
        assert!(build_args(&input(Action::Commit)).is_err());
    }

    #[test]
    fn rejects_option_injection() {
        let mut checkout = input(Action::Checkout);
        checkout.target = Some("--orphan".into());
        assert!(build_args(&checkout).is_err());
    }

    #[test]
    fn push_supports_upstream() {
        let mut push = input(Action::Push);
        push.set_upstream = true;
        push.branch = Some("main".into());
        let (args, read_only, _, _) = build_args(&push).unwrap();
        assert!(!read_only);
        assert!(args.iter().any(|arg| arg == "-u"));
        assert_eq!(args.last().unwrap(), "main");
    }
}
