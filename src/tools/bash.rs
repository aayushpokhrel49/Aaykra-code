//! Shell execution with timeouts, cancellation, output limits, and sandboxing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{SandboxConfig, SandboxMode};
use crate::permissions::{CommandRisk, classify_command};
use crate::tools::{Tool, ToolContext, definition, permission_request, truncate_tool_output};
use crate::types::{RiskLevel, ToolDefinition};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    description: Option<String>,
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Shell command to run with `bash -lc`. Avoid interactive commands."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1000,
                "maximum": MAX_TIMEOUT_MS,
                "description": "Kill the command after this many milliseconds. Default 120000."
            },
            "workdir": {
                "type": "string",
                "description": "Working directory, relative to the project or absolute. Defaults to the session cwd."
            },
            "env": {
                "type": "object",
                "additionalProperties": {"type": "string"},
                "description": "Extra environment variables for this command."
            },
            "description": {
                "type": "string",
                "description": "Short human-readable purpose shown in the UI and permission prompt."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            "Bash",
            "Run a shell command and capture stdout, stderr, and the exit code. Commands are sandboxed when bubblewrap is available (filesystem writes limited to the project; network follows sandbox config), run with a timeout, and can be cancelled. Prefer dedicated tools for reading, searching, and editing files. Never run interactive commands.",
            schema(),
        )
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> Result<String> {
        context.ensure_not_cancelled()?;
        let input: BashInput = serde_json::from_value(input).context("invalid Bash input")?;
        if input.command.trim().is_empty() {
            bail!("command must not be empty");
        }
        let timeout = Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1_000, MAX_TIMEOUT_MS),
        );
        let workdir = match input.workdir.as_deref() {
            Some(raw) if !raw.trim().is_empty() => {
                let path = context.resolve_path(raw)?;
                if !path.is_dir() {
                    bail!("workdir is not a directory: {}", path.display());
                }
                path
            }
            _ => context.cwd(),
        };
        let risk = match classify_command(&input.command) {
            CommandRisk::Safe => RiskLevel::Low,
            CommandRisk::Caution => RiskLevel::Medium,
            CommandRisk::Destructive => RiskLevel::High,
        };
        let reason = input
            .description
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("run a shell command");
        let mut request = permission_request(
            "Bash",
            input.command.clone(),
            reason,
            false,
            risk,
            vec![workdir.clone()],
        );
        request.suggested_rule = format!(
            "Bash({}:*)",
            input
                .command
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        );
        context.authorize(request).await?;

        let sandbox = Sandbox::select(context.runtime.config.sandbox.clone())?;
        let mut command =
            sandbox.command(&input.command, &workdir, &context.runtime.config.sandbox);
        command
            .current_dir(&workdir)
            .envs(&input.env)
            .env("AAYKRA_SESSION_ID", context.runtime.session_id())
            .env("AAYKRA_AGENT", &context.agent_name)
            .env("AAYKRA_CWD", workdir.display().to_string())
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let started = Instant::now();
        let output = tokio::select! {
            result = tokio::time::timeout(timeout, command.output()) => {
                match result {
                    Ok(result) => result.context("could not run shell command")?,
                    Err(_) => bail!(
                        "command timed out after {:.1}s and was killed: {}",
                        timeout.as_secs_f64(),
                        input.command
                    ),
                }
            }
            () = context.runtime.cancel.cancelled() => {
                bail!("command cancelled by user");
            }
        };
        let elapsed = started.elapsed();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut body = String::new();
        if !stdout.trim().is_empty() {
            body.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("stderr:\n");
            body.push_str(stderr.trim_end());
        }
        if body.trim().is_empty() {
            body.push_str("(no output)");
        }
        let code = output
            .status
            .code()
            .map_or_else(|| "signal".to_string(), |code| code.to_string());
        let suffix = sandbox.label();
        let mut rendered = format!(
            "$ {}\n{body}\n\n[exit {code} · {:.2}s{}{suffix}]",
            input.command,
            elapsed.as_secs_f64(),
            if output.status.success() {
                ""
            } else {
                " · failed"
            },
        );
        if !output.status.success() {
            rendered.push_str("\nThe command failed. Inspect stderr and adjust before retrying.");
        }
        Ok(truncate_tool_output(context, rendered))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sandbox {
    None,
    Bubblewrap,
    Seatbelt,
}

impl Sandbox {
    fn select(config: SandboxConfig) -> Result<Self> {
        match config.mode {
            SandboxMode::Off => Ok(Self::None),
            SandboxMode::Bubblewrap => {
                if cfg!(target_os = "linux") && bwrap_available() {
                    Ok(Self::Bubblewrap)
                } else {
                    bail!(
                        "sandbox mode is `bubblewrap` but bwrap is unavailable; install bubblewrap or set sandbox.mode = \"off\""
                    )
                }
            }
            SandboxMode::Seatbelt => {
                if cfg!(target_os = "macos") && sandbox_exec_available() {
                    Ok(Self::Seatbelt)
                } else {
                    bail!(
                        "sandbox mode is `seatbelt` but sandbox-exec is unavailable; this mode requires macOS"
                    )
                }
            }
            SandboxMode::Auto => {
                if cfg!(target_os = "linux") && bwrap_available() {
                    Ok(Self::Bubblewrap)
                } else if cfg!(target_os = "macos") && sandbox_exec_available() {
                    Ok(Self::Seatbelt)
                } else {
                    Ok(Self::None)
                }
            }
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Bubblewrap => " · bwrap",
            Self::Seatbelt => " · seatbelt",
        }
    }

    fn command(self, script: &str, cwd: &Path, config: &SandboxConfig) -> tokio::process::Command {
        let shell = shell_path();
        match self {
            Self::None => {
                let mut command = tokio::process::Command::new(shell);
                command.arg("-lc").arg(script);
                command
            }
            Self::Bubblewrap => {
                let mut command = tokio::process::Command::new("bwrap");
                command.args(bwrap_args(cwd, config));
                command.arg("--").arg(shell).arg("-lc").arg(script);
                command
            }
            Self::Seatbelt => {
                let profile = if config.allow_network {
                    "(version 1)\n(allow default)\n"
                } else {
                    "(version 1)\n(allow default)\n(deny network*)\n"
                };
                let mut command = tokio::process::Command::new("sandbox-exec");
                command
                    .arg("-p")
                    .arg(profile)
                    .arg(shell)
                    .arg("-lc")
                    .arg(script);
                command
            }
        }
    }
}

fn bwrap_args(cwd: &Path, config: &SandboxConfig) -> Vec<String> {
    let cwd_string = cwd.to_string_lossy().to_string();
    let mut args: Vec<String> = vec![
        "--die-with-parent".into(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--chdir".into(),
        cwd_string.clone(),
        "--bind".into(),
        cwd_string,
        cwd.to_string_lossy().to_string(),
    ];
    if !config.allow_network {
        args.push("--unshare-net".into());
    }
    for path in &config.extra_writable {
        let path = crate::paths::expand_tilde(&path.to_string_lossy());
        if path.exists() {
            let value = path.to_string_lossy().to_string();
            args.extend(["--bind".into(), value.clone(), value]);
        }
    }
    for path in &config.extra_readonly {
        let path = crate::paths::expand_tilde(&path.to_string_lossy());
        if path.exists() {
            let value = path.to_string_lossy().to_string();
            args.extend(["--ro-bind".into(), value.clone(), value]);
        }
    }
    args
}

fn shell_path() -> &'static str {
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| {
        if PathBuf::from("/bin/bash").is_file() {
            "/bin/bash"
        } else {
            "sh"
        }
    })
}

fn bwrap_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| probe("bwrap"))
}

fn sandbox_exec_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| probe("sandbox-exec"))
}

fn probe(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bwrap_args_bind_cwd_and_respect_network() {
        let config = SandboxConfig::default();
        let args = bwrap_args(Path::new("/work"), &config);
        assert!(args.windows(2).any(|pair| pair == ["--bind", "/work"]));
        assert!(!args.iter().any(|arg| arg == "--unshare-net"));
        let mut offline = config;
        offline.allow_network = false;
        let args = bwrap_args(Path::new("/work"), &offline);
        assert!(args.iter().any(|arg| arg == "--unshare-net"));
    }

    #[test]
    fn classifies_risk_levels() {
        assert_eq!(classify_command("git status"), CommandRisk::Safe);
        assert_eq!(classify_command("ls && cat x"), CommandRisk::Caution);
        assert_eq!(classify_command("rm -rf /"), CommandRisk::Destructive);
    }
}
