//! Tree-sitter powered structural repository map.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Bash,
}

impl CodeLanguage {
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "go" => Some(Self::Go),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            _ => None,
        }
    }

    fn language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::Bash => "shell",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    pub line: usize,
    pub end_line: usize,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct FileMap {
    pub path: String,
    pub language: CodeLanguage,
    pub symbols: Vec<Symbol>,
}

#[derive(Debug, Clone, Default)]
pub struct RepoMap {
    pub files: Vec<FileMap>,
    pub skipped_files: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct RepoMapOptions {
    pub max_files: usize,
    pub max_symbols_per_file: usize,
    pub max_file_bytes: u64,
}

impl Default for RepoMapOptions {
    fn default() -> Self {
        Self {
            max_files: 1_500,
            max_symbols_per_file: 160,
            max_file_bytes: 1_500_000,
        }
    }
}

impl RepoMap {
    pub fn build(root: &Path, options: &RepoMapOptions) -> Result<Self> {
        let mut map = Self::default();
        let walker = WalkBuilder::new(root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().is_some_and(|t| t.is_dir()) {
                    return !matches!(
                        name.as_ref(),
                        "target"
                            | "node_modules"
                            | ".venv"
                            | "venv"
                            | "dist"
                            | "build"
                            | "vendor"
                            | ".next"
                            | ".turbo"
                            | "__pycache__"
                    );
                }
                true
            })
            .build();

        let mut parser = Parser::new();
        let mut candidates = Vec::new();
        for entry in walker.flatten() {
            let path = entry.path();
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if CodeLanguage::from_path(path).is_none() {
                continue;
            }
            candidates.push(path.to_path_buf());
            if candidates.len() >= options.max_files {
                map.truncated = true;
                break;
            }
        }
        candidates.sort();

        for path in candidates {
            let Some(language) = CodeLanguage::from_path(&path) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&path) else {
                map.skipped_files += 1;
                continue;
            };
            if metadata.len() > options.max_file_bytes {
                map.skipped_files += 1;
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                map.skipped_files += 1;
                continue;
            };
            if bytes.iter().take(8_000).any(|byte| *byte == 0) {
                map.skipped_files += 1;
                continue;
            }
            let Ok(source) = std::str::from_utf8(&bytes) else {
                map.skipped_files += 1;
                continue;
            };
            if parser.set_language(&language.language()).is_err() {
                map.skipped_files += 1;
                continue;
            }
            let Some(tree) = parser.parse(source, None) else {
                map.skipped_files += 1;
                continue;
            };
            let symbols = extract_symbols(language, tree.root_node(), source.as_bytes(), options);
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            map.files.push(FileMap {
                path: relative,
                language,
                symbols,
            });
        }
        Ok(map)
    }

    #[must_use]
    pub fn render(&self, max_chars: usize) -> String {
        let mut out = format!(
            "Repository map: {} source files{}.\n",
            self.files.len(),
            if self.truncated {
                " (truncated; increase context.repo_map_files for more)"
            } else {
                ""
            }
        );
        out.push_str("Use Read to inspect details; symbol line numbers are 1-based.\n\n");
        for file in &self.files {
            if out.len() >= max_chars {
                out.push_str("… map truncated\n");
                break;
            }
            if file.symbols.is_empty() {
                continue;
            }
            out.push_str(&file.path);
            out.push('\n');
            for symbol in &file.symbols {
                let indent = "  ".repeat(symbol.depth.min(6));
                let line = format!(
                    "{indent}{} {}:{} (line {})\n",
                    symbol.kind, symbol.name, symbol.line, symbol.end_line
                );
                if out.len() + line.len() > max_chars {
                    out.push_str("… map truncated\n");
                    return out;
                }
                out.push_str(&line);
            }
        }
        out
    }

    #[must_use]
    pub fn search(&self, query: &str, max_chars: usize) -> String {
        let needle = query.to_ascii_lowercase();
        let mut out = String::new();
        for file in &self.files {
            let matching: Vec<&Symbol> = file
                .symbols
                .iter()
                .filter(|symbol| {
                    symbol.name.to_ascii_lowercase().contains(&needle)
                        || file.path.to_ascii_lowercase().contains(&needle)
                })
                .collect();
            if matching.is_empty() {
                continue;
            }
            out.push_str(&file.path);
            out.push('\n');
            for symbol in matching {
                out.push_str(&format!(
                    "  {} {} (line {}-{})\n",
                    symbol.kind, symbol.name, symbol.line, symbol.end_line
                ));
                if out.len() > max_chars {
                    out.push_str("… results truncated\n");
                    return out;
                }
            }
        }
        if out.is_empty() {
            format!("No structural symbols or files matched `{query}`.")
        } else {
            out
        }
    }
}

fn extract_symbols(
    language: CodeLanguage,
    root: Node<'_>,
    source: &[u8],
    options: &RepoMapOptions,
) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    walk_node(language, root, source, 0, &mut symbols, options);
    symbols
}

fn walk_node(
    language: CodeLanguage,
    node: Node<'_>,
    source: &[u8],
    depth: usize,
    symbols: &mut Vec<Symbol>,
    options: &RepoMapOptions,
) {
    if symbols.len() >= options.max_symbols_per_file {
        return;
    }
    let kind = node.kind();
    if let Some((label, name)) = symbol_for_node(language, node, kind, source) {
        let start = node.start_position().row + 1;
        let end = node.end_position().row + 1;
        symbols.push(Symbol {
            name,
            kind: label,
            line: start,
            end_line: end,
            depth,
        });
    }
    let mut cursor = node.walk();
    let child_depth = if is_scope_kind(kind) {
        depth + 1
    } else {
        depth
    };
    for child in node.children(&mut cursor) {
        walk_node(language, child, source, child_depth, symbols, options);
    }
}

fn is_scope_kind(kind: &str) -> bool {
    matches!(
        kind,
        "impl_item"
            | "trait_item"
            | "class_definition"
            | "class_declaration"
            | "interface_declaration"
            | "mod_item"
            | "namespace_definition"
    )
}

fn symbol_for_node(
    language: CodeLanguage,
    node: Node<'_>,
    kind: &str,
    source: &[u8],
) -> Option<(&'static str, String)> {
    let (label, field) = match language {
        CodeLanguage::Rust => match kind {
            "function_item" => ("fn", "name"),
            "struct_item" => ("struct", "name"),
            "enum_item" => ("enum", "name"),
            "trait_item" => ("trait", "name"),
            "impl_item" => ("impl", "type"),
            "mod_item" => ("mod", "name"),
            "type_item" => ("type", "name"),
            "macro_definition" => ("macro", "name"),
            _ => return None,
        },
        CodeLanguage::Python => match kind {
            "function_definition" => ("def", "name"),
            "class_definition" => ("class", "name"),
            "decorated_definition" => return None,
            _ => return None,
        },
        CodeLanguage::JavaScript | CodeLanguage::TypeScript | CodeLanguage::Tsx => match kind {
            "function_declaration" => ("fn", "name"),
            "class_declaration" => ("class", "name"),
            "method_definition" => ("method", "name"),
            "interface_declaration" => ("interface", "name"),
            "type_alias_declaration" => ("type", "name"),
            "enum_declaration" => ("enum", "name"),
            "lexical_declaration" => return None,
            "public_field_definition" => ("field", "name"),
            _ => return None,
        },
        CodeLanguage::Go => match kind {
            "function_declaration" => ("fn", "name"),
            "method_declaration" => ("method", "name"),
            "type_declaration" => ("type", "name"),
            "type_spec" => ("type", "name"),
            _ => return None,
        },
        CodeLanguage::Bash => match kind {
            "function_definition" => ("fn", "name"),
            _ => return None,
        },
    };
    let name_node = node
        .child_by_field_name(field)
        .or_else(|| node.child_by_field_name("name"))?;
    let name = name_node.utf8_text(source).ok()?.trim().to_string();
    if name.is_empty() || name.len() > 120 {
        return None;
    }
    Some((label, name))
}

/// Return a compact one-line git-ish view of files and top-level symbols.
#[must_use]
pub fn quick_overview(root: &Path) -> Result<String> {
    let map = RepoMap::build(root, &RepoMapOptions::default())?;
    Ok(map.render(20_000))
}

/// Build a repository overview, reusing a recent cache when possible. The cache
/// is keyed by the canonical project path and map limits, so resuming a session
/// or switching models does not re-parse the whole tree.
pub fn quick_overview_cached(
    root: &Path,
    cache_dir: &Path,
    max_files: usize,
    max_chars: usize,
    ttl: Duration,
) -> Result<String> {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher.update(b":");
    hasher.update(max_files.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(max_chars.to_string().as_bytes());
    let key = hex::encode(hasher.finalize());
    let directory = cache_dir.join("repo-map");
    let cache_file = directory.join(format!("{key}.txt"));

    if !ttl.is_zero()
        && let Ok(metadata) = std::fs::metadata(&cache_file)
        && metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age <= ttl)
        && let Ok(text) = std::fs::read_to_string(&cache_file)
        && !text.trim().is_empty()
    {
        return Ok(text);
    }

    let options = RepoMapOptions {
        max_files: max_files.max(1),
        ..RepoMapOptions::default()
    };
    let map = RepoMap::build(root, &options)?;
    let text = map.render(max_chars);
    if !ttl.is_zero() && !text.trim().is_empty() {
        let _ = std::fs::create_dir_all(&directory);
        let _ = crate::paths::atomic_write(&cache_file, text.as_bytes());
    }
    Ok(text)
}

#[must_use]
pub fn relative_display(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

#[allow(dead_code)]
fn _path_buf(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rust_symbols() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("lib.rs"),
            "pub struct Widget;\nimpl Widget {\n    pub fn new() -> Self { Self }\n}\npub fn main() {}\n",
        )
        .unwrap();
        let map = RepoMap::build(root.path(), &RepoMapOptions::default()).unwrap();
        assert_eq!(map.files.len(), 1);
        let names: Vec<&str> = map.files[0]
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"Widget"));
        assert!(names.contains(&"new"));
        assert!(names.contains(&"main"));
    }

    #[test]
    fn parses_python_and_search() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("app.py"),
            "class Service:\n    def run(self):\n        pass\n\ndef helper():\n    pass\n",
        )
        .unwrap();
        let map = RepoMap::build(root.path(), &RepoMapOptions::default()).unwrap();
        let search = map.search("service", 10_000);
        assert!(search.contains("Service"));
        assert!(search.contains("app.py"));
    }

    #[test]
    fn cached_overview_reuses_a_fresh_cache() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.rs"), "pub fn first() {}\n").unwrap();
        let first = quick_overview_cached(
            root.path(),
            cache.path(),
            50,
            4_000,
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(first.contains("first"));

        std::fs::write(root.path().join("lib.rs"), "pub fn second() {}\n").unwrap();
        let cached = quick_overview_cached(
            root.path(),
            cache.path(),
            50,
            4_000,
            Duration::from_secs(60),
        )
        .unwrap();
        assert_eq!(cached, first);

        let refreshed =
            quick_overview_cached(root.path(), cache.path(), 50, 4_000, Duration::ZERO).unwrap();
        assert!(refreshed.contains("second"));
    }

    #[test]
    fn ignores_node_modules() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        std::fs::write(root.path().join("node_modules/pkg/x.js"), "function x() {}").unwrap();
        std::fs::write(root.path().join("index.js"), "function y() {}").unwrap();
        let map = RepoMap::build(root.path(), &RepoMapOptions::default()).unwrap();
        assert!(map.files.iter().all(|f| !f.path.contains("node_modules")));
    }
}
