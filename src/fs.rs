//! Applying multi-file patches to files in a directory.
//!
//! This is the I/O layer over the in-memory apply APIs: it resolves the file
//! names of a [`Patch`] against a directory (with GNU patch-style protection
//! against paths escaping it), handles file creation, deletion and git
//! renames, detects already-applied diffs, applies what fits GNU patch-style
//! and writes `<file>.rej` files for hunks that do not apply.
//!
//! ```no_run
//! use flickzeug::{Patch, fs::{DirApplyOptions, apply_patch_dir}};
//!
//! let patch_text = std::fs::read("changes.patch")?;
//! let patch = Patch::from_bytes(&patch_text)?;
//!
//! let report = apply_patch_dir("src".as_ref(), &patch, &DirApplyOptions::default())?;
//! for file in &report {
//!     println!("{}: {:?}", file.path.display(), file.outcome);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::{
    io,
    path::{Component, Path, PathBuf},
};

use crate::{
    ApplyConfig, ApplyOutcome, ApplyStats, Diff, Patch, apply_bytes_partial, apply_bytes_reporting,
};

/// Options for [`apply_patch_dir`]
#[derive(Debug, Clone)]
pub struct DirApplyOptions {
    /// Per-hunk application configuration (fuzzy matching, line endings)
    pub apply_config: ApplyConfig,
    /// Strip this many leading path components from the file names in the
    /// patch, in addition to the conventional `a/` `b/` prefixes the parser
    /// already strips
    pub strip: usize,
    /// Apply the patch in reverse
    pub reverse: bool,
    /// Report what would happen without touching the filesystem
    pub dry_run: bool,
    /// Write `<file>.rej` files containing the hunks that failed to apply,
    /// like GNU patch (default `true`); with `false` rejected hunks are only
    /// reported in the [`FileOutcome`]
    pub write_rejects: bool,
}

impl Default for DirApplyOptions {
    fn default() -> Self {
        Self {
            apply_config: ApplyConfig::default(),
            strip: 0,
            reverse: false,
            dry_run: false,
            write_rejects: true,
        }
    }
}

/// What happened to one file while applying a patch with [`apply_patch_dir`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOutcome {
    /// The file was patched cleanly (created if it did not exist)
    Patched {
        /// Statistics over the applied hunks
        stats: ApplyStats,
        /// Whether the file was newly created
        created: bool,
    },
    /// One or more hunks did not apply; the ones that did were written (GNU
    /// patch behavior) and the rest were saved to `reject_file` when
    /// [`DirApplyOptions::write_rejects`] is on
    HunksRejected {
        /// Statistics over the hunks that did apply
        stats: ApplyStats,
        /// How many hunks were rejected
        rejected: usize,
        /// Where the rejected hunks were written, if they were
        reject_file: Option<PathBuf>,
    },
    /// The diff is already applied to this file; nothing was changed
    AlreadyApplied,
    /// The file was deleted (its diff removed all content)
    Deleted,
    /// The file was renamed without content changes
    Renamed {
        /// The new path
        to: PathBuf,
    },
    /// A metadata-only diff with no content changes; nothing was done
    Unchanged,
}

impl FileOutcome {
    /// Whether this outcome means the patch did not fully apply
    pub fn is_failure(&self) -> bool {
        matches!(self, FileOutcome::HunksRejected { .. })
    }
}

/// The per-file report of [`apply_patch_dir`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFile {
    /// The file the outcome refers to, as resolved inside the directory
    pub path: PathBuf,
    /// What happened to it
    pub outcome: FileOutcome,
}

/// An error that aborts [`apply_patch_dir`]
///
/// Hunks that merely fail to apply are *not* errors — they are reported as
/// [`FileOutcome::HunksRejected`] so the remaining files still get patched,
/// like GNU patch does.
#[derive(Debug, thiserror::Error)]
pub enum DirApplyError {
    /// A file name in the patch is not valid UTF-8
    #[error("patch contains a non-UTF-8 file name")]
    NonUtf8Filename,
    /// Stripping [`DirApplyOptions::strip`] components left nothing
    #[error("nothing left of file name {0:?} after stripping {1} leading components")]
    EmptyFilename(String, usize),
    /// The file name is absolute or escapes the target directory
    #[error("refusing to touch unsafe path {0:?}")]
    UnsafePath(String),
    /// A filesystem operation failed
    #[error("{path}: {source}")]
    Io {
        /// The path the operation failed on
        path: PathBuf,
        /// The underlying I/O error
        #[source]
        source: io::Error,
    },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> DirApplyError + '_ {
    move |source| DirApplyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Resolve a file name from the patch against `directory`, stripping `strip`
/// leading components and rejecting absolute paths and `..` components.
fn resolve_path(directory: &Path, name: &[u8], strip: usize) -> Result<PathBuf, DirApplyError> {
    let name = std::str::from_utf8(name).map_err(|_| DirApplyError::NonUtf8Filename)?;
    let path: PathBuf = Path::new(name).components().skip(strip).collect();

    if path.as_os_str().is_empty() {
        return Err(DirApplyError::EmptyFilename(name.to_owned(), strip));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(DirApplyError::UnsafePath(name.to_owned())),
        }
    }

    // Avoid a cosmetic "./" prefix when resolving against the current dir
    if directory == Path::new(".") {
        Ok(path)
    } else {
        Ok(directory.join(path))
    }
}

/// Apply a (multi-file) patch to the files in `directory`.
///
/// File names are resolved like `patch`/`git apply` would: the parser already
/// strips the conventional `a/` `b/` prefixes, [`DirApplyOptions::strip`]
/// removes additional leading components, and paths that are absolute or
/// escape `directory` abort with [`DirApplyError::UnsafePath`].
///
/// Per file this handles creation (`/dev/null` old side), deletion (the file
/// is removed when the diff empties it), git renames (with or without content
/// changes), already-applied detection (the file is left alone and reported
/// as [`FileOutcome::AlreadyApplied`]), and partial application: hunks that
/// fit are applied and the rest are written to `<file>.rej` — the same
/// behavior as GNU patch.
///
/// Returns one [`AppliedFile`] per file diff, in patch order. The result is
/// `Err` only for environment problems (I/O failures, unsafe paths); hunks
/// that do not apply are reported in the corresponding [`FileOutcome`].
pub fn apply_patch_dir(
    directory: &Path,
    patch: &Patch<'_, [u8]>,
    options: &DirApplyOptions,
) -> Result<Vec<AppliedFile>, DirApplyError> {
    let mut report = Vec::with_capacity(patch.len());

    for diff in patch {
        let reversed;
        let diff = if options.reverse {
            reversed = diff.reverse();
            &reversed
        } else {
            diff
        };

        let original = diff
            .original()
            .map(|name| resolve_path(directory, name, options.strip))
            .transpose()?;
        let modified = diff
            .modified()
            .map(|name| resolve_path(directory, name, options.strip))
            .transpose()?;

        match (original, modified) {
            (None, None) => {}
            // Pure rename or metadata-only diff without hunks
            (Some(from), Some(to)) if diff.hunks().is_empty() => {
                if from == to {
                    report.push(AppliedFile {
                        path: from,
                        outcome: FileOutcome::Unchanged,
                    });
                } else {
                    if !options.dry_run {
                        if let Some(parent) = to.parent() {
                            std::fs::create_dir_all(parent).map_err(io_err(&to))?;
                        }
                        std::fs::rename(&from, &to).map_err(io_err(&from))?;
                    }
                    report.push(AppliedFile {
                        path: from,
                        outcome: FileOutcome::Renamed { to },
                    });
                }
            }
            // File deletion: apply the hunks and remove the file if empty
            (Some(from), None) => {
                let base = std::fs::read(&from).map_err(io_err(&from))?;
                let outcome = match apply_bytes_reporting(&base, diff, &options.apply_config) {
                    ApplyOutcome::Applied(content, _) if content.is_empty() => {
                        if !options.dry_run {
                            std::fs::remove_file(&from).map_err(io_err(&from))?;
                        }
                        FileOutcome::Deleted
                    }
                    // The diff claims deletion but content remains: keep the
                    // patched file (GNU patch behaves the same without -E)
                    ApplyOutcome::Applied(content, stats) => {
                        if !options.dry_run {
                            std::fs::write(&from, content).map_err(io_err(&from))?;
                        }
                        FileOutcome::Patched {
                            stats,
                            created: false,
                        }
                    }
                    ApplyOutcome::AlreadyApplied(_) => FileOutcome::AlreadyApplied,
                    ApplyOutcome::Failed(_) => reject_outcome(diff, &from, &base, options)?,
                };
                report.push(AppliedFile {
                    path: from,
                    outcome,
                });
            }
            // File creation or modification (possibly a rename)
            (original, Some(to)) => {
                let source = match &original {
                    Some(from) if from.exists() => Some(from.clone()),
                    _ if to.exists() => Some(to.clone()),
                    _ => None,
                };
                let base = match &source {
                    Some(path) => std::fs::read(path).map_err(io_err(path))?,
                    None => Vec::new(),
                };
                let outcome = apply_one(diff, &to, &base, source.is_some(), options)?;

                // A rename with content changes: the patched content was
                // written to `to`; remove the old file
                if !matches!(outcome, FileOutcome::AlreadyApplied)
                    && let Some(from) = &original
                    && *from != to
                    && from.exists()
                    && !options.dry_run
                {
                    std::fs::remove_file(from).map_err(io_err(from))?;
                }

                report.push(AppliedFile { path: to, outcome });
            }
        }
    }

    Ok(report)
}

/// Apply one diff against `base` and write the result to `target`,
/// GNU patch-style: what fits is applied, the rest goes to `<target>.rej`.
fn apply_one(
    diff: &Diff<'_, [u8]>,
    target: &Path,
    base: &[u8],
    target_existed: bool,
    options: &DirApplyOptions,
) -> Result<FileOutcome, DirApplyError> {
    let write = |content: &[u8]| -> Result<(), DirApplyError> {
        if options.dry_run {
            return Ok(());
        }
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_err(target))?;
        }
        std::fs::write(target, content).map_err(io_err(target))
    };

    match apply_bytes_reporting(base, diff, &options.apply_config) {
        ApplyOutcome::Applied(content, stats) => {
            write(&content)?;
            Ok(FileOutcome::Patched {
                stats,
                created: !target_existed,
            })
        }
        ApplyOutcome::AlreadyApplied(_) => Ok(FileOutcome::AlreadyApplied),
        ApplyOutcome::Failed(_) => reject_outcome(diff, target, base, options),
    }
}

/// GNU patch behavior for a diff whose hunks do not all apply: apply the ones
/// that fit, save the rest to `<target>.rej`.
fn reject_outcome(
    diff: &Diff<'_, [u8]>,
    target: &Path,
    base: &[u8],
    options: &DirApplyOptions,
) -> Result<FileOutcome, DirApplyError> {
    let partial = apply_bytes_partial(base, diff, &options.apply_config);
    if partial.stats.hunks_applied > 0 && !options.dry_run {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(io_err(target))?;
        }
        std::fs::write(target, &partial.content).map_err(io_err(target))?;
    }

    let reject_file = if options.write_rejects && !options.dry_run {
        let path = reject_path(target);
        let reject_diff = Diff::new(diff.original(), diff.modified(), partial.rejected.clone());
        std::fs::write(&path, reject_diff.to_bytes()).map_err(io_err(&path))?;
        Some(path)
    } else {
        None
    };

    Ok(FileOutcome::HunksRejected {
        stats: partial.stats,
        rejected: partial.rejected.len(),
        reject_file,
    })
}

/// `<file>.rej`, like GNU patch's default reject file name
fn reject_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".rej");
    target.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "flickzeug-fs-test-{}-{}-{}",
            name,
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn modify_create_delete_and_rename() {
        let dir = scratch_dir("basic");
        std::fs::write(dir.join("modify.txt"), "line 1\nline 2\nline 3\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "old stuff\n").unwrap();
        std::fs::write(dir.join("old-name.txt"), "moving\n").unwrap();

        let patch_text = "\
--- a/modify.txt
+++ b/modify.txt
@@ -1,3 +1,3 @@
 line 1
-line 2
+line two
 line 3
--- /dev/null
+++ b/fresh.txt
@@ -0,0 +1,1 @@
+created
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-old stuff
diff --git a/old-name.txt b/new-name.txt
similarity index 100%
rename from old-name.txt
rename to new-name.txt
";
        let patch = Patch::from_bytes(patch_text.as_bytes()).unwrap();
        let report = apply_patch_dir(&dir, &patch, &DirApplyOptions::default()).unwrap();

        assert_eq!(report.len(), 4);
        assert!(matches!(
            report[0].outcome,
            FileOutcome::Patched { created: false, .. }
        ));
        assert!(matches!(
            report[1].outcome,
            FileOutcome::Patched { created: true, .. }
        ));
        assert_eq!(report[2].outcome, FileOutcome::Deleted);
        assert!(matches!(report[3].outcome, FileOutcome::Renamed { .. }));

        assert_eq!(
            std::fs::read_to_string(dir.join("modify.txt")).unwrap(),
            "line 1\nline two\nline 3\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("fresh.txt")).unwrap(),
            "created\n"
        );
        assert!(!dir.join("gone.txt").exists());
        assert!(!dir.join("old-name.txt").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("new-name.txt")).unwrap(),
            "moving\n"
        );

        // Applying again: the content diffs report AlreadyApplied
        std::fs::write(dir.join("old-name.txt"), "moving\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "old stuff\n").unwrap();
        let report = apply_patch_dir(&dir, &patch, &DirApplyOptions::default()).unwrap();
        assert_eq!(report[0].outcome, FileOutcome::AlreadyApplied);
        assert_eq!(report[1].outcome, FileOutcome::AlreadyApplied);
    }

    #[test]
    fn partial_application_writes_rejects() {
        let dir = scratch_dir("rejects");
        std::fs::write(dir.join("f.txt"), "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n").unwrap();

        // First hunk applies, second one's context does not exist
        let patch_text = "\
--- f.txt
+++ f.txt
@@ -1,3 +1,3 @@
 a
-b
+B
 c
@@ -8,3 +8,3 @@
 WRONG
-CONTEXT
+NOPE
 HERE
";
        let patch = Patch::from_bytes(patch_text.as_bytes()).unwrap();
        let report = apply_patch_dir(&dir, &patch, &DirApplyOptions::default()).unwrap();

        assert_eq!(report.len(), 1);
        let FileOutcome::HunksRejected {
            stats,
            rejected,
            reject_file,
        } = &report[0].outcome
        else {
            panic!("expected HunksRejected, got {:?}", report[0].outcome);
        };
        assert_eq!(stats.hunks_applied, 1);
        assert_eq!(*rejected, 1);

        // The good hunk was applied (GNU patch behavior)
        assert!(
            std::fs::read_to_string(dir.join("f.txt"))
                .unwrap()
                .starts_with("a\nB\nc\n")
        );

        // The reject file contains exactly the failed hunk
        let reject = std::fs::read_to_string(reject_file.as_ref().unwrap()).unwrap();
        assert_eq!(
            reject,
            "--- f.txt\n+++ f.txt\n@@ -8,3 +8,3 @@\n WRONG\n-CONTEXT\n+NOPE\n HERE\n"
        );
    }

    #[test]
    fn dry_run_touches_nothing() {
        let dir = scratch_dir("dry-run");
        std::fs::write(dir.join("f.txt"), "x\n").unwrap();
        let patch_text = "--- f.txt\n+++ f.txt\n@@ -1 +1 @@\n-x\n+y\n";
        let patch = Patch::from_bytes(patch_text.as_bytes()).unwrap();

        let options = DirApplyOptions {
            dry_run: true,
            ..DirApplyOptions::default()
        };
        let report = apply_patch_dir(&dir, &patch, &options).unwrap();
        assert!(matches!(report[0].outcome, FileOutcome::Patched { .. }));
        assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "x\n");
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        let dir = scratch_dir("unsafe");
        let patch_text = "--- a/../escape\n+++ b/../escape\n@@ -0,0 +1 @@\n+x\n";
        let patch = Patch::from_bytes(patch_text.as_bytes()).unwrap();
        let err = apply_patch_dir(&dir, &patch, &DirApplyOptions::default()).unwrap_err();
        assert!(matches!(err, DirApplyError::UnsafePath(_)), "{err}");
    }
}
