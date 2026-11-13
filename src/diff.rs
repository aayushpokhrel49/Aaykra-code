//! Human-readable unified diffs used for permission previews and editor UI.

use similar::TextDiff;

const MAX_DIFF_CHARS: usize = 80_000;

#[must_use]
pub fn unified_diff(path: &str, old: &str, new: &str, context: usize) -> String {
    if old == new {
        return String::new();
    }
    let diff = TextDiff::from_lines(old, new);
    let mut output = diff
        .unified_diff()
        .context_radius(context)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    if !output.ends_with('\n') {
        output.push('\n');
    }
    if output.len() > MAX_DIFF_CHARS {
        let truncated: String = output.chars().take(MAX_DIFF_CHARS).collect();
        output = format!(
            "{truncated}\n… diff truncated for display ({} bytes)\n",
            output.len()
        );
    }
    output
}

#[must_use]
pub fn added_removed(diff: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            added += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

#[must_use]
pub fn diff_summary(diff: &str) -> String {
    let (added, removed) = added_removed(diff);
    format!("+{added} −{removed}")
}

/// Parse a unified diff into individual hunks. Editor clients can use this to
/// offer per-hunk review while the agent continues to treat the edit as one
/// atomic tool action.
#[must_use]
pub fn split_hunks(diff: &str) -> Vec<String> {
    let mut hunks = Vec::new();
    let mut current: Option<String> = None;
    let mut header = String::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            if let Some(existing) = current.take() {
                hunks.push(existing);
            }
            let mut hunk = header.clone();
            hunk.push_str(line);
            hunk.push('\n');
            current = Some(hunk);
        } else if line.starts_with("--- ") || line.starts_with("+++ ") {
            header.push_str(line);
            header.push('\n');
        } else if let Some(hunk) = current.as_mut() {
            hunk.push_str(line);
            hunk.push('\n');
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_unified_diff() {
        let diff = unified_diff("src/main.rs", "one\ntwo\n", "one\nthree\n", 3);
        assert!(diff.contains("-two"));
        assert!(diff.contains("+three"));
        assert_eq!(added_removed(&diff), (1, 1));
    }

    #[test]
    fn splits_hunks() {
        let old = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
        let mut new = old.replace("line 2\n", "changed 2\n");
        new = new.replace("line 22\n", "changed 22\n");
        let diff = unified_diff("x", &old, &new, 1);
        let hunks = split_hunks(&diff);
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn identical_text_has_no_diff() {
        assert!(unified_diff("x", "same\n", "same\n", 3).is_empty());
    }
}
