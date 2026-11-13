//! Filesystem conventions for Aaykra Code.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const APP_DIR: &str = ".aaykra";
pub const CONFIG_FILE: &str = "config.toml";
pub const CREDENTIALS_FILE: &str = "credentials.toml";
pub const SESSIONS_DIR: &str = "sessions";
pub const CHECKPOINTS_DIR: &str = "checkpoints";
pub const LOGS_DIR: &str = "logs";
pub const SKILLS_DIR: &str = "skills";
pub const AGENTS_DIR: &str = "agents";
pub const MEMORY_FILE: &str = "MEMORY.md";
pub const GLOBAL_INSTRUCTIONS_FILE: &str = "AAYKRA.md";

#[derive(Debug, Clone)]
pub struct AaykraPaths {
    /// `~/.aaykra` (or `$AAYKRA_HOME`).
    pub home: PathBuf,
    pub sessions: PathBuf,
    pub checkpoints: PathBuf,
    pub logs: PathBuf,
    pub skills: PathBuf,
    pub agents: PathBuf,
    pub cache: PathBuf,
}

impl AaykraPaths {
    pub fn discover() -> Result<Self> {
        let home = if let Ok(value) = std::env::var("AAYKRA_HOME") {
            expand_tilde(&value)
        } else {
            dirs::home_dir()
                .context("could not determine home directory")?
                .join(APP_DIR)
        };
        let cache = dirs::cache_dir()
            .unwrap_or_else(|| home.join("cache"))
            .join("aaykra");
        let paths = Self {
            sessions: home.join(SESSIONS_DIR),
            checkpoints: home.join(CHECKPOINTS_DIR),
            logs: home.join(LOGS_DIR),
            skills: home.join(SKILLS_DIR),
            agents: home.join(AGENTS_DIR),
            cache,
            home,
        };
        paths.ensure()?;
        Ok(paths)
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            &self.home,
            &self.sessions,
            &self.checkpoints,
            &self.logs,
            &self.skills,
            &self.agents,
            &self.cache,
        ] {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(())
    }

    #[must_use]
    pub fn global_config(&self) -> PathBuf {
        self.home.join(CONFIG_FILE)
    }

    #[must_use]
    pub fn credentials(&self) -> PathBuf {
        self.home.join(CREDENTIALS_FILE)
    }

    #[must_use]
    pub fn session_file(&self, id: &str) -> PathBuf {
        self.sessions.join(format!("{id}.json"))
    }

    #[must_use]
    pub fn checkpoint_dir(&self, session_id: &str, turn: u64) -> PathBuf {
        self.checkpoints
            .join(session_id)
            .join(format!("turn-{turn}"))
    }

    #[must_use]
    pub fn global_memory(&self) -> PathBuf {
        self.home.join(MEMORY_FILE)
    }

    #[must_use]
    pub fn global_instructions(&self) -> PathBuf {
        self.home.join(GLOBAL_INSTRUCTIONS_FILE)
    }
}

/// Global config path without creating any directories (config loading is
/// allowed to run in read-only environments).
pub fn global_config_path_from_env() -> Result<PathBuf> {
    if let Ok(value) = std::env::var("AAYKRA_GLOBAL_CONFIG") {
        return Ok(expand_tilde(&value));
    }
    if let Ok(value) = std::env::var("AAYKRA_HOME") {
        return Ok(expand_tilde(&value).join(CONFIG_FILE));
    }
    let home = dirs::home_dir().context("could not determine home directory")?;
    Ok(home.join(APP_DIR).join(CONFIG_FILE))
}

#[must_use]
pub fn project_agent_dir(project: &Path) -> PathBuf {
    project.join(APP_DIR).join("agents")
}

#[must_use]
pub fn project_skills_dir(project: &Path) -> PathBuf {
    project.join(APP_DIR).join("skills")
}

#[must_use]
pub fn project_config_candidates(project: &Path) -> Vec<PathBuf> {
    vec![
        project.join(APP_DIR).join(CONFIG_FILE),
        project.join("aaykra.toml"),
    ]
}

#[must_use]
pub fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
pub fn secure_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("could not secure {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn secure_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

/// Convert an arbitrary path into a safe flat filename component.
#[must_use]
pub fn path_component(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    while value.starts_with('/') {
        value.remove(0);
    }
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_is_flat() {
        assert_eq!(path_component(Path::new("/tmp/a b/c.rs")), "tmp_a_b_c.rs");
    }

    #[test]
    fn tilde_expands() {
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_tilde("~/x"), home.join("x"));
        }
    }
}
