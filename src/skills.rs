//! Discoverable, Markdown-based skills.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indexmap::IndexMap;

use crate::frontmatter::{MarkdownDoc, list_field, parse};
use crate::paths::AaykraPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    Builtin,
    Global,
    Project,
}

impl SkillSource {
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
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub allowed_tools: Vec<String>,
    pub source: SkillSource,
    pub path: Option<PathBuf>,
}

impl Skill {
    #[must_use]
    pub fn prompt(&self) -> String {
        let mut out = format!(
            "The user invoked the `{}` skill. Follow these workflow instructions.\n\n",
            self.name
        );
        if !self.allowed_tools.is_empty() {
            out.push_str(&format!(
                "Suggested tools: {}\n\n",
                self.allowed_tools.join(", ")
            ));
        }
        out.push_str(&self.body);
        out
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: IndexMap<String, Skill>,
}

impl SkillRegistry {
    pub fn discover(project_root: &Path, paths: &AaykraPaths) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut skills = IndexMap::new();
        for (name, description, tools, body) in builtin_skills() {
            skills.insert(
                name.to_string(),
                Skill {
                    name: name.to_string(),
                    description: description.to_string(),
                    body: body.to_string(),
                    allowed_tools: tools
                        .split(',')
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .collect(),
                    source: SkillSource::Builtin,
                    path: None,
                },
            );
        }

        for (root, source) in [
            (paths.skills.clone(), SkillSource::Global),
            (
                crate::paths::project_skills_dir(project_root),
                SkillSource::Project,
            ),
        ] {
            for skill in load_directory(&root, source, &mut warnings) {
                if let Some(existing) = skills.get(&skill.name)
                    && existing.source != SkillSource::Builtin
                {
                    warnings.push(format!(
                        "skill `{}` from {} overrides {}",
                        skill.name,
                        skill.source.label(),
                        existing.source.label()
                    ));
                }
                skills.insert(skill.name.clone(), skill);
            }
        }
        skills.sort_keys();
        (Self { skills }, warnings)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.skills.keys().map(String::as_str).collect()
    }

    #[must_use]
    pub fn all(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    #[must_use]
    pub fn render_catalog(&self) -> String {
        if self.skills.is_empty() {
            return "No skills installed.".into();
        }
        let mut lines = vec!["Skills:".to_string()];
        for skill in self.skills.values() {
            lines.push(format!(
                "  /{:<24} {:<48} [{}]",
                skill.name,
                truncate(&skill.description, 48),
                skill.source.label()
            ));
        }
        lines.join("\n")
    }
}

fn load_directory(root: &Path, source: SkillSource, warnings: &mut Vec<String>) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join("SKILL.md");
            if candidate.is_file() {
                candidates.push(candidate);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            candidates.push(path);
        }
    }
    candidates.sort();
    let mut skills = Vec::new();
    for path in candidates {
        match load_skill(&path, source) {
            Ok(skill) => skills.push(skill),
            Err(error) => warnings.push(format!("skill {}: {error:#}", path.display())),
        }
    }
    skills
}

fn load_skill(path: &Path, source: SkillSource) -> Result<Skill> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let MarkdownDoc { frontmatter, body } = parse(&text)?;
    let fallback_name = path
        .parent()
        .filter(|parent| parent.file_name().and_then(|n| n.to_str()) != Some("skills"))
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or("skill");
    let raw_name = frontmatter
        .get("name")
        .cloned()
        .unwrap_or_else(|| fallback_name.to_string());
    let name = sanitize_name(&raw_name);
    if name.is_empty() {
        anyhow::bail!("skill has no valid name");
    }
    if body.trim().is_empty() {
        anyhow::bail!("skill `{name}` has an empty body");
    }
    let description = frontmatter.get("description").cloned().unwrap_or_else(|| {
        body.lines()
            .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
            .unwrap_or("Custom skill")
            .trim()
            .to_string()
    });
    let tools = frontmatter
        .get("allowed_tools")
        .or_else(|| frontmatter.get("tools"))
        .map(|value| list_field(value))
        .unwrap_or_default();
    Ok(Skill {
        name,
        description,
        body,
        allowed_tools: tools,
        source,
        path: Some(path.to_path_buf()),
    })
}

fn sanitize_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else if ch == ' ' {
                '-'
            } else {
                '\0'
            }
        })
        .filter(|ch| *ch != '\0')
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

fn builtin_skills() -> &'static [(&'static str, &'static str, &'static str, &'static str)] {
    &[
        (
            "code-review",
            "Review working-tree changes for correctness, security, and regressions.",
            "Read,Grep,Glob,Bash(git diff:*),Bash(git status:*)",
            "Review the current working tree like a senior engineer.\n\n\
             1. Run `git status --short` and inspect the full diff (including staged changes).\n\
             2. Read surrounding code for each behavioral change; do not review the diff in isolation.\n\
             3. Look specifically for correctness bugs, security issues, broken edge cases, race conditions, data loss, and missing tests.\n\
             4. Report findings ordered by severity with concrete `file:line` references and a suggested fix.\n\
             5. Do not modify files unless the user explicitly asks you to apply fixes. Never praise or pad the review.\n",
        ),
        (
            "commit",
            "Create small, high-quality conventional commits without pushing.",
            "Bash(git status:*),Bash(git diff:*),Bash(git log:*)",
            "Create one or more commits for the current changes.\n\n\
             - Inspect status, staged diff, and recent log style first.\n\
             - Split unrelated changes into logical commits; do not commit secrets or generated junk.\n\
             - Use an imperative conventional-commit subject (feat, fix, refactor, perf, test, docs, chore).\n\
             - Include a short body only when it explains intent, not a line-by-line restatement.\n\
             - Never push, amend public history, rebase, or commit unrelated files unless explicitly asked.\n",
        ),
        (
            "debug",
            "Systematically reproduce, isolate, and fix a bug with a regression test.",
            "Read,Grep,Glob,Edit,Bash",
            "Debug methodically, not by guessing.\n\n\
             1. Establish a concrete reproduction and the expected behavior.\n\
             2. Trace the actual control/data flow; inspect logs and relevant callers.\n\
             3. Form a falsifiable hypothesis and test the smallest relevant slice.\n\
             4. Fix the root cause, not the symptom.\n\
             5. Add a regression test that fails before the fix and passes after it.\n\
             6. Run the narrowest relevant checks first, then broader checks if practical.\n",
        ),
        (
            "security-review",
            "Audit a diff or codebase for exploitable security issues.",
            "Read,Grep,Glob",
            "Perform a focused security audit.\n\n\
             Review trust boundaries, authentication/authorization, injection sinks, path traversal, SSRF, unsafe deserialization, secrets handling, command execution, dependency risk, and DoS limits. Report only issues supported by concrete code paths, with severity and file references. Avoid speculative checklists.\n",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> AaykraPaths {
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
    fn builtins_are_available() {
        let paths = temp_paths();
        let (registry, warnings) = SkillRegistry::discover(Path::new("/tmp"), &paths);
        assert!(registry.get("code-review").is_some());
        assert!(warnings.is_empty());
    }

    #[test]
    fn project_skill_overrides_global() {
        let paths = temp_paths();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(paths.skills.join("deploy")).unwrap();
        std::fs::write(
            paths.skills.join("deploy").join("SKILL.md"),
            "---\nname: deploy\ndescription: global\n---\nGLOBAL",
        )
        .unwrap();
        let dir = project.path().join(".aaykra/skills/deploy");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: project\n---\nPROJECT",
        )
        .unwrap();
        let (registry, _) = SkillRegistry::discover(project.path(), &paths);
        let skill = registry.get("deploy").unwrap();
        assert_eq!(skill.description, "project");
        assert_eq!(skill.body, "PROJECT");
    }
}
