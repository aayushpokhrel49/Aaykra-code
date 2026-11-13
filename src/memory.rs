//! Project instructions and persistent memory (`MEMORY.md`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::paths::{AaykraPaths, atomic_write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Global,
    Project,
}

impl MemoryScope {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryStore {
    pub global_path: PathBuf,
    pub project_path: PathBuf,
}

impl MemoryStore {
    #[must_use]
    pub fn new(project_root: &Path, paths: &AaykraPaths) -> Self {
        Self {
            global_path: paths.global_memory(),
            project_path: project_root.join(crate::paths::APP_DIR).join("MEMORY.md"),
        }
    }

    #[must_use]
    pub fn path_for(&self, scope: MemoryScope) -> &Path {
        match scope {
            MemoryScope::Global => &self.global_path,
            MemoryScope::Project => &self.project_path,
        }
    }

    pub fn load(&self) -> Result<String> {
        let mut sections = Vec::new();
        for (scope, path) in [
            (MemoryScope::Global, &self.global_path),
            (MemoryScope::Project, &self.project_path),
        ] {
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("could not read memory {}", path.display()))?;
            if !text.trim().is_empty() {
                sections.push(format!(
                    "### {} memory ({})\n{}",
                    scope.label(),
                    path.display(),
                    text.trim()
                ));
            }
        }
        Ok(sections.join("\n\n"))
    }

    pub fn add(&self, scope: MemoryScope, note: &str) -> Result<()> {
        let note = note.trim();
        if note.is_empty() {
            anyhow::bail!("memory note is empty");
        }
        if note.len() > 4_000 {
            anyhow::bail!("memory note is too long (max 4000 characters)");
        }
        let path = self.path_for(scope);
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        if existing.contains(note) {
            return Ok(());
        }
        let heading = "# Aaykra Code Memory\n\nDurable project knowledge. Keep entries concise and non-secret.\n\n## Learnings\n";
        let mut content = if existing.trim().is_empty() {
            heading.to_string()
        } else {
            existing.clone()
        };
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&format!(
            "- {} — {}\n",
            Utc::now().format("%Y-%m-%d"),
            note.lines().collect::<Vec<_>>().join(" ")
        ));
        atomic_write(path, content.as_bytes())
    }

    pub fn remove_matching(&self, query: &str) -> Result<Vec<String>> {
        let query_lower = query.to_ascii_lowercase();
        let mut removed = Vec::new();
        for scope in [MemoryScope::Global, MemoryScope::Project] {
            let path = self.path_for(scope);
            if !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(path)?;
            let mut kept = Vec::new();
            for line in text.lines() {
                if line.trim_start().starts_with("- ")
                    && line.to_ascii_lowercase().contains(&query_lower)
                {
                    removed.push(format!("[{}] {}", scope.label(), line.trim()));
                } else {
                    kept.push(line);
                }
            }
            if !removed.is_empty() {
                atomic_write(path, format!("{}\n", kept.join("\n")).as_bytes())?;
            }
        }
        Ok(removed)
    }

    #[must_use]
    pub fn render(&self) -> String {
        match self.load() {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => {
                "No persistent memories yet. Add one with `/memory add <note>` or the Memory tool."
                    .into()
            }
            Err(error) => format!("Could not load memory: {error:#}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProjectInstructions {
    pub sources: Vec<PathBuf>,
    pub content: String,
}

pub fn load_project_instructions(
    config: &crate::config::Config,
    project_root: &Path,
    paths: &AaykraPaths,
) -> ProjectInstructions {
    let mut sources = Vec::new();
    let mut sections = Vec::new();
    for path in config.instructions_paths(project_root, paths) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        sections.push(format!(
            "Instructions from `{}`:\n\n{}",
            path.display(),
            text.trim()
        ));
        sources.push(path);
    }
    ProjectInstructions {
        sources,
        content: sections.join("\n\n---\n\n"),
    }
}

/// Extract obvious "remember this" requests from a user prompt. This is
/// intentionally conservative; the Memory tool handles nuanced cases.
#[must_use]
pub fn auto_memory_candidate(prompt: &str) -> Option<String> {
    let prompt = prompt.trim();
    let lower = prompt.to_ascii_lowercase();
    let markers = [
        "remember that ",
        "remember: ",
        "please remember ",
        "always remember ",
        "keep in mind that ",
    ];
    for marker in markers {
        if let Some(position) = lower.find(marker) {
            let note = prompt[position + marker.len()..].trim();
            if !note.is_empty() && note.len() <= 500 {
                return Some(note.to_string());
            }
        }
    }
    None
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
    fn adds_and_loads_memory() {
        let root = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(root.path(), &paths());
        store
            .add(MemoryScope::Project, "Use thiserror for library errors")
            .unwrap();
        let content = store.load().unwrap();
        assert!(content.contains("Use thiserror"));
    }

    #[test]
    fn detects_remember_requests() {
        assert_eq!(
            auto_memory_candidate("Please remember that tests use nextest"),
            Some("tests use nextest".into())
        );
        assert_eq!(auto_memory_candidate("fix the bug"), None);
    }
}
