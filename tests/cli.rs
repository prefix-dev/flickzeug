//! Integration tests for the `flickzeug` CLI (requires the `cli` feature).

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU32, Ordering},
};

static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Create a unique scratch directory for one test.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "flickzeug-cli-test-{}-{}-{}",
        name,
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn flickzeug(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flickzeug"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("failed to run flickzeug binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn diff_identical_files_exits_zero() {
    let dir = scratch_dir("diff-same");
    fs::write(dir.join("a.txt"), "same\ncontent\n").unwrap();
    fs::write(dir.join("b.txt"), "same\ncontent\n").unwrap();

    let output = flickzeug(&["diff", "a.txt", "b.txt"], &dir);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout(&output).is_empty());
}

#[test]
fn diff_differing_files_prints_unified_diff() {
    let dir = scratch_dir("diff-changed");
    fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    fs::write(dir.join("b.txt"), "one\n2\nthree\n").unwrap();

    let output = flickzeug(&["diff", "a.txt", "b.txt"], &dir);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("--- a.txt"), "{text}");
    assert!(text.contains("+++ b.txt"), "{text}");
    assert!(text.contains("-two"), "{text}");
    assert!(text.contains("+2"), "{text}");
}

#[test]
fn diff_output_applies_back() {
    let dir = scratch_dir("roundtrip");
    fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    fs::write(dir.join("b.txt"), "alpha\nBETA\ngamma\ndelta\n").unwrap();

    let output = flickzeug(&["diff", "a.txt", "b.txt"], &dir);
    assert_eq!(output.status.code(), Some(1));
    fs::write(dir.join("change.patch"), &output.stdout).unwrap();

    // The diff names the files a.txt/b.txt, so applying it patches... b.txt.
    // Copy the original into place first to patch "in a fresh tree".
    let tree = scratch_dir("roundtrip-tree");
    fs::write(tree.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    fs::write(tree.join("b.txt"), "alpha\nbeta\ngamma\n").unwrap();

    let patch_path = dir.join("change.patch");
    let output = flickzeug(&["apply", patch_path.to_str().unwrap()], &tree);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        fs::read_to_string(tree.join("b.txt")).unwrap(),
        "alpha\nBETA\ngamma\ndelta\n"
    );
}

#[test]
fn apply_patches_modifies_creates_and_deletes() {
    let dir = scratch_dir("apply");
    fs::write(dir.join("existing.txt"), "line 1\nline 2\nline 3\n").unwrap();
    fs::write(dir.join("obsolete.txt"), "old stuff\n").unwrap();
    let patch = "\
--- a/existing.txt
+++ b/existing.txt
@@ -1,3 +1,3 @@
 line 1
-line 2
+line two
 line 3
--- /dev/null
+++ b/brand_new.txt
@@ -0,0 +1,2 @@
+created
+file
--- a/obsolete.txt
+++ /dev/null
@@ -1 +0,0 @@
-old stuff
";
    fs::write(dir.join("change.patch"), patch).unwrap();

    let output = flickzeug(&["apply", "change.patch"], &dir);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        fs::read_to_string(dir.join("existing.txt")).unwrap(),
        "line 1\nline two\nline 3\n"
    );
    assert_eq!(
        fs::read_to_string(dir.join("brand_new.txt")).unwrap(),
        "created\nfile\n"
    );
    assert!(!dir.join("obsolete.txt").exists());

    // Applying the same patch again must be detected and skipped
    // (obsolete.txt is gone, so only run the modify hunk's file again).
    let repatch = "\
--- a/existing.txt
+++ b/existing.txt
@@ -1,3 +1,3 @@
 line 1
-line 2
+line two
 line 3
";
    fs::write(dir.join("again.patch"), repatch).unwrap();
    let output = flickzeug(&["apply", "again.patch"], &dir);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(stdout(&output).contains("already applied"), "{output:?}");
}

#[test]
fn apply_reverse_undoes_a_patch() {
    let dir = scratch_dir("reverse");
    fs::write(dir.join("f.txt"), "new\n").unwrap();
    let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1 +1 @@
-old
+new
";
    fs::write(dir.join("p.patch"), patch).unwrap();

    let output = flickzeug(&["apply", "--reverse", "p.patch"], &dir);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fs::read_to_string(dir.join("f.txt")).unwrap(), "old\n");
}

#[test]
fn apply_failure_exits_one() {
    let dir = scratch_dir("apply-fail");
    fs::write(dir.join("f.txt"), "completely unrelated\n").unwrap();
    let patch = "\
--- a/f.txt
+++ b/f.txt
@@ -1,3 +1,3 @@
 context before
-old
+new
 context after
";
    fs::write(dir.join("p.patch"), patch).unwrap();

    let output = flickzeug(&["apply", "p.patch"], &dir);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    // GNU patch-style reporting and reject file
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("1 out of 1 hunk FAILED -- saving rejects to file"),
        "{stderr}"
    );
    let reject = fs::read_to_string(dir.join("f.txt.rej")).unwrap();
    assert!(reject.contains("@@ -1,3 +1,3 @@"), "{reject}");
    // The base file is untouched (no hunks applied)
    assert_eq!(
        fs::read_to_string(dir.join("f.txt")).unwrap(),
        "completely unrelated\n"
    );
}

#[test]
fn apply_rejects_path_traversal() {
    let dir = scratch_dir("apply-traversal");
    let patch = "\
--- a/../escape.txt
+++ b/../escape.txt
@@ -0,0 +1 @@
+gotcha
";
    fs::write(dir.join("p.patch"), patch).unwrap();

    let output = flickzeug(&["apply", "p.patch"], &dir);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(!dir.parent().unwrap().join("escape.txt").exists());
}

#[test]
fn merge_clean_and_conflicting() {
    let dir = scratch_dir("merge");
    fs::write(dir.join("base.txt"), "a\nb\nc\n").unwrap();
    fs::write(dir.join("ours.txt"), "A\nb\nc\n").unwrap();
    fs::write(dir.join("theirs.txt"), "a\nb\nC\n").unwrap();

    let output = flickzeug(&["merge", "ours.txt", "base.txt", "theirs.txt"], &dir);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(stdout(&output), "A\nb\nC\n");

    // Both sides change the same line differently -> conflict, exit 1
    fs::write(dir.join("theirs.txt"), "different\nb\nc\n").unwrap();
    let output = flickzeug(&["merge", "ours.txt", "base.txt", "theirs.txt"], &dir);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    assert!(text.contains("<<<<<<<"), "{text}");
    assert!(text.contains(">>>>>>>"), "{text}");
}
