//! Parser for the traditional (normal) diff format.
//!
//! Normal diff format uses commands like:
//! - `NaR` - add lines R from new file after line N in old file
//! - `NcR` - change lines N in old file to lines R from new file
//! - `NdR` - delete lines N from old file (would appear after line R in new file)
//!
//! Where N and R are line numbers or ranges like `start,end`.
//!
//! Lines from the old file are prefixed with `< ` and lines from the new file
//! are prefixed with `> `. Change commands have a `---` separator between the
//! old and new lines.

use super::{Diff, Hunk, HunkRange, Line};
use crate::utils::{LineIter, Text};

use super::parse::ParsePatchError;

type Result<T, E = ParsePatchError> = std::result::Result<T, E>;

/// Detect whether the input looks like a normal diff (as opposed to unified).
///
/// Returns `true` if the first non-empty line matches the pattern
/// `\d+(,\d+)?[acd]\d+(,\d+)?`.
pub fn is_normal_diff<T: Text + ?Sized>(input: &T) -> bool {
    let first_line = LineIter::new(input).next();
    if let Some((line, _end)) = first_line
        && let Some(s) = line.as_str()
    {
        return parse_command_line(s).is_some();
    }
    false
}

/// Parse a normal diff format string into a `Diff`.
pub fn parse_normal(input: &str) -> Result<Diff<'_, str>> {
    let hunks = parse_normal_hunks(input)?;
    Ok(Diff::new(None::<&str>, None::<&str>, hunks))
}

/// Parse a normal diff format byte slice into a `Diff`.
pub fn parse_normal_bytes(input: &[u8]) -> Result<Diff<'_, [u8]>> {
    let hunks = parse_normal_hunks(input)?;
    Ok(Diff::new(None::<&[u8]>, None::<&[u8]>, hunks))
}

/// Parse multiple normal diffs (not really applicable for normal format,
/// but provided for API consistency). Normal diff doesn't have multi-file
/// support built in, so this just returns a single diff.
pub fn parse_normal_multiple(input: &str) -> Result<Vec<Diff<'_, str>>> {
    Ok(vec![parse_normal(input)?])
}

/// Parse multiple normal diffs from bytes.
pub fn parse_normal_bytes_multiple(input: &[u8]) -> Result<Vec<Diff<'_, [u8]>>> {
    Ok(vec![parse_normal_bytes(input)?])
}

/// A parsed command line from a normal diff.
#[derive(Debug, Clone, Copy)]
struct NormalCommand {
    old_start: usize,
    old_end: usize,
    command: char,
    new_start: usize,
    new_end: usize,
}

/// Parse a command line like `3c3`, `1,2d5`, `0a1,3`.
fn parse_command_line(line: &str) -> Option<NormalCommand> {
    // Find the command character (a, c, or d)
    let cmd_pos = line.find(['a', 'c', 'd'])?;
    let command = line.as_bytes()[cmd_pos] as char;

    let left = &line[..cmd_pos];
    let right = &line[cmd_pos + 1..];

    let (old_start, old_end) = parse_range(left)?;
    let (new_start, new_end) = parse_range(right)?;

    Some(NormalCommand {
        old_start,
        old_end,
        command,
        new_start,
        new_end,
    })
}

/// Parse a range like `3` or `1,5`.
fn parse_range(s: &str) -> Option<(usize, usize)> {
    if let Some((start, end)) = s.split_once(',') {
        let start: usize = start.parse().ok()?;
        let end: usize = end.parse().ok()?;
        Some((start, end))
    } else {
        let n: usize = s.parse().ok()?;
        Some((n, n))
    }
}

fn parse_normal_hunks<'a, T: Text + ?Sized + ToOwned>(input: &'a T) -> Result<Vec<Hunk<'a, T>>> {
    let all_lines: Vec<_> = LineIter::new(input).collect();
    let mut hunks = Vec::new();
    let mut i = 0;

    while i < all_lines.len() {
        let (line, _end) = all_lines[i];

        // Try to parse as a command line
        let line_str = line.as_str().ok_or(ParsePatchError::HunkHeader)?;

        // Skip empty lines
        if line_str.trim().is_empty() {
            i += 1;
            continue;
        }

        let cmd = parse_command_line(line_str).ok_or(ParsePatchError::HunkHeader)?;
        i += 1;

        let mut lines: Vec<Line<'a, T>> = Vec::new();

        match cmd.command {
            'a' => {
                // Add: lines from new file prefixed with "> "
                while i < all_lines.len() {
                    let (l, _) = all_lines[i];
                    if let Some(content) = l.strip_prefix("> ") {
                        lines.push(Line::Insert((content, all_lines[i].1)));
                        i += 1;
                    } else {
                        break;
                    }
                }

                let old_range = HunkRange::new(cmd.old_start + 1, 0);
                let new_range = HunkRange::new(cmd.new_start, cmd.new_end - cmd.new_start + 1);
                hunks.push(Hunk::new(old_range, new_range, None, lines));
            }
            'd' => {
                // Delete: lines from old file prefixed with "< "
                while i < all_lines.len() {
                    let (l, _) = all_lines[i];
                    if let Some(content) = l.strip_prefix("< ") {
                        lines.push(Line::Delete((content, all_lines[i].1)));
                        i += 1;
                    } else {
                        break;
                    }
                }

                let old_range = HunkRange::new(cmd.old_start, cmd.old_end - cmd.old_start + 1);
                let new_range = HunkRange::new(cmd.new_start, 0);
                hunks.push(Hunk::new(old_range, new_range, None, lines));
            }
            'c' => {
                // Change: old lines with "< ", then "---", then new lines with "> "
                while i < all_lines.len() {
                    let (l, _) = all_lines[i];
                    if let Some(content) = l.strip_prefix("< ") {
                        lines.push(Line::Delete((content, all_lines[i].1)));
                        i += 1;
                    } else {
                        break;
                    }
                }

                // Expect "---" separator
                if i < all_lines.len() {
                    let (l, _) = all_lines[i];
                    if l.as_str() == Some("---") {
                        i += 1;
                    } else {
                        return Err(ParsePatchError::HunkHeader);
                    }
                } else {
                    return Err(ParsePatchError::UnexpectedEof);
                }

                while i < all_lines.len() {
                    let (l, _) = all_lines[i];
                    if let Some(content) = l.strip_prefix("> ") {
                        lines.push(Line::Insert((content, all_lines[i].1)));
                        i += 1;
                    } else {
                        break;
                    }
                }

                let old_range = HunkRange::new(cmd.old_start, cmd.old_end - cmd.old_start + 1);
                let new_range = HunkRange::new(cmd.new_start, cmd.new_end - cmd.new_start + 1);
                hunks.push(Hunk::new(old_range, new_range, None, lines));
            }
            _ => return Err(ParsePatchError::HunkHeader),
        }
    }

    if hunks.is_empty() {
        return Err(ParsePatchError::NoHunks);
    }

    Ok(hunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply;
    use std::path::PathBuf;

    fn test_data_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join("normal-diff")
    }

    #[test]
    fn test_is_normal_diff() {
        assert!(is_normal_diff("2c2\n< old\n---\n> new\n"));
        assert!(is_normal_diff("1,3d0\n< a\n< b\n< c\n"));
        assert!(is_normal_diff("0a1,2\n> x\n> y\n"));
        assert!(!is_normal_diff("--- a/file\n+++ b/file\n"));
        assert!(!is_normal_diff("@@ -1,3 +1,3 @@\n"));
        assert!(!is_normal_diff("diff --git a/f b/f\n"));
    }

    #[test]
    fn test_parse_change_and_add() {
        let dir = test_data_dir();
        let old = std::fs::read_to_string(dir.join("old1.txt")).unwrap();
        let new = std::fs::read_to_string(dir.join("new1.txt")).unwrap();
        let patch_str = std::fs::read_to_string(dir.join("change_and_add.diff")).unwrap();

        let diff = parse_normal(&patch_str).unwrap();
        assert_eq!(diff.hunks().len(), 2);

        let (result, stats) = apply(&old, &diff).unwrap();
        assert_eq!(result, new);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_parse_delete_insert_delete() {
        let dir = test_data_dir();
        let old = std::fs::read_to_string(dir.join("old2.txt")).unwrap();
        let new = std::fs::read_to_string(dir.join("new2.txt")).unwrap();
        let patch_str = std::fs::read_to_string(dir.join("delete_insert_delete.diff")).unwrap();

        let diff = parse_normal(&patch_str).unwrap();
        assert_eq!(diff.hunks().len(), 3);

        let (result, stats) = apply(&old, &diff).unwrap();
        assert_eq!(result, new);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_parse_add_only() {
        let dir = test_data_dir();
        let old = std::fs::read_to_string(dir.join("old3.txt")).unwrap();
        let new = std::fs::read_to_string(dir.join("new3.txt")).unwrap();
        let patch_str = std::fs::read_to_string(dir.join("add_only.diff")).unwrap();

        let diff = parse_normal(&patch_str).unwrap();
        assert_eq!(diff.hunks().len(), 2);

        let (result, stats) = apply(&old, &diff).unwrap();
        assert_eq!(result, new);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_parse_complex() {
        let dir = test_data_dir();
        let old = std::fs::read_to_string(dir.join("old4.txt")).unwrap();
        let new = std::fs::read_to_string(dir.join("new4.txt")).unwrap();
        let patch_str = std::fs::read_to_string(dir.join("complex.diff")).unwrap();

        let diff = parse_normal(&patch_str).unwrap();

        let (result, stats) = apply(&old, &diff).unwrap();
        assert_eq!(result, new);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_parse_inline_change() {
        let patch = "2c2\n< old line\n---\n> new line\n";
        let diff = parse_normal(patch).unwrap();
        assert_eq!(diff.hunks().len(), 1);

        let hunk = &diff.hunks()[0];
        assert_eq!(hunk.old_range().start(), 2);
        assert_eq!(hunk.old_range().len(), 1);
        assert_eq!(hunk.new_range().start(), 2);
        assert_eq!(hunk.new_range().len(), 1);
        assert_eq!(hunk.lines().len(), 2);
    }

    #[test]
    fn test_parse_inline_delete() {
        let patch = "2,3d1\n< line two\n< line three\n";
        let diff = parse_normal(patch).unwrap();
        assert_eq!(diff.hunks().len(), 1);

        let hunk = &diff.hunks()[0];
        assert_eq!(hunk.old_range().start(), 2);
        assert_eq!(hunk.old_range().len(), 2);
        assert_eq!(hunk.new_range().start(), 1);
        assert_eq!(hunk.new_range().len(), 0);
        assert_eq!(hunk.lines().len(), 2);
    }

    #[test]
    fn test_parse_inline_add() {
        let patch = "0a1,2\n> added one\n> added two\n";
        let diff = parse_normal(patch).unwrap();
        assert_eq!(diff.hunks().len(), 1);

        let hunk = &diff.hunks()[0];
        assert_eq!(hunk.old_range().start(), 1);
        assert_eq!(hunk.old_range().len(), 0);
        assert_eq!(hunk.new_range().start(), 1);
        assert_eq!(hunk.new_range().len(), 2);
        assert_eq!(hunk.lines().len(), 2);
    }

    #[test]
    fn test_parse_bytes() {
        let patch = b"2c2\n< old\n---\n> new\n";
        let diff = parse_normal_bytes(patch).unwrap();
        assert_eq!(diff.hunks().len(), 1);
    }

    #[test]
    fn test_roundtrip_change() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nmodified line 2\nline 3\n";
        let patch = "2c2\n< line 2\n---\n> modified line 2\n";

        let diff = parse_normal(patch).unwrap();
        let (result, _) = apply(old, &diff).unwrap();
        assert_eq!(result, new);
    }

    #[test]
    fn test_roundtrip_delete() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nline 3\n";
        let patch = "2d1\n< line 2\n";

        let diff = parse_normal(patch).unwrap();
        let (result, _) = apply(old, &diff).unwrap();
        assert_eq!(result, new);
    }

    #[test]
    fn test_roundtrip_add() {
        let old = "line 1\nline 3\n";
        let new = "line 1\nline 2\nline 3\n";
        let patch = "1a2\n> line 2\n";

        let diff = parse_normal(patch).unwrap();
        let (result, _) = apply(old, &diff).unwrap();
        assert_eq!(result, new);
    }

    #[test]
    fn test_multiline_change() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nC\nd\n";
        let patch = "2,3c2,3\n< b\n< c\n---\n> B\n> C\n";

        let diff = parse_normal(patch).unwrap();
        let (result, stats) = apply(old, &diff).unwrap();
        assert_eq!(result, new);
        assert_eq!(stats.lines_added, 2);
        assert_eq!(stats.lines_deleted, 2);
    }
}
