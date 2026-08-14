//! End-to-end comparison against GNU diff, GNU patch, and git.
//!
//! These tests run the `flickzeug` CLI side by side with the reference tools
//! and assert that output, resulting files, and exit codes agree. Patches are
//! also cross-applied: flickzeug-generated diffs must be consumable by GNU
//! patch and `git apply`, and GNU/git-generated diffs by `flickzeug apply`.
//!
//! Tests skip (with a note on stderr) when a reference tool is missing, so
//! the suite is safe on minimal systems; CI images have all three.
//!
//! Known, deliberate divergences from GNU patch (not tested here):
//! - deleted files are removed when they end up empty (like `git apply`);
//!   GNU patch keeps empty files unless run with -E
//! - `-p/--strip` counts components *after* the automatic a/ b/ prefix strip

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU32, Ordering},
};

static DIR_COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "flickzeug-compat-{}-{}-{}",
        name,
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn have(tool: &str) -> bool {
    let found = Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found {
        eprintln!("skipping: `{tool}` not available");
    }
    found
}

fn run(cmd: &mut Command) -> Output {
    cmd.output()
        .unwrap_or_else(|err| panic!("failed to run {cmd:?}: {err}"))
}

fn flickzeug(args: &[&str], cwd: &Path) -> Output {
    run(Command::new(env!("CARGO_BIN_EXE_flickzeug"))
        .args(args)
        .current_dir(cwd))
}

fn stdout_string(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// GNU diff labels the files with `\t<timestamp>`; strip that so the headers
/// are comparable with flickzeug's (which prints no timestamps).
fn normalize_headers(diff: &str) -> String {
    diff.lines()
        .map(|line| {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                line.split('\t').next().unwrap()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// File pairs whose unified diffs must be byte-identical between GNU diff and
/// flickzeug (after header normalization).
fn curated_pairs() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "replace-middle",
            "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
            "one\ntwo\nTHREE\nfour\nfive\nsix\nseven\nEIGHT\nnine\nten\neleven\n",
        ),
        (
            "empty-context-lines",
            "top\n\nmid\n\nbottom\n",
            "top\n\nMID\n\nbottom\n",
        ),
        (
            "change-at-start",
            "first\nsecond\nthird\nfourth\nfifth\n",
            "FIRST\nsecond\nthird\nfourth\nfifth\n",
        ),
        (
            "change-at-end",
            "first\nsecond\nthird\nfourth\nfifth\n",
            "first\nsecond\nthird\nfourth\nFIFTH\n",
        ),
        (
            "no-trailing-newline",
            "alpha\nbeta\ngamma",
            "alpha\nBETA\ngamma",
        ),
        (
            "gain-trailing-newline",
            "alpha\nbeta\ngamma",
            "alpha\nbeta\ngamma\n",
        ),
        ("insert-only", "a\nb\nc\n", "a\nb\nnew\nc\n"),
        ("delete-only", "a\nb\nc\nd\n", "a\nb\nd\n"),
        ("create-from-empty", "", "fresh\ncontent\n"),
        ("delete-everything", "doomed\ncontent\n", ""),
        (
            "two-hunks",
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n20\n",
            "1\nTWO\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n14\n15\n16\n17\n18\nNINETEEN\n20\n",
        ),
    ]
}

#[test]
fn diff_output_identical_to_gnu_diff() {
    if !have("diff") {
        return;
    }

    for (name, old, new) in curated_pairs() {
        let dir = scratch_dir("gnu-diff");
        fs::write(dir.join("old.txt"), old).unwrap();
        fs::write(dir.join("new.txt"), new).unwrap();

        let gnu = run(Command::new("diff")
            .args(["-u", "old.txt", "new.txt"])
            .current_dir(&dir));
        let flick = flickzeug(&["diff", "old.txt", "new.txt"], &dir);

        assert_eq!(
            gnu.status.code(),
            flick.status.code(),
            "case {name}: exit codes differ"
        );
        assert_eq!(
            normalize_headers(&stdout_string(&gnu)),
            normalize_headers(&stdout_string(&flick)),
            "case {name}: diff output differs from GNU diff"
        );
    }
}

#[test]
fn diff_exit_codes_match_gnu_diff() {
    if !have("diff") {
        return;
    }

    let dir = scratch_dir("exit-codes");
    fs::write(dir.join("same1.txt"), "equal\n").unwrap();
    fs::write(dir.join("same2.txt"), "equal\n").unwrap();
    fs::write(dir.join("other.txt"), "different\n").unwrap();

    // identical -> 0, different -> 1, missing file -> 2
    for (a, b, expected) in [
        ("same1.txt", "same2.txt", 0),
        ("same1.txt", "other.txt", 1),
        ("same1.txt", "missing.txt", 2),
    ] {
        let gnu = run(Command::new("diff").args(["-u", a, b]).current_dir(&dir));
        let flick = flickzeug(&["diff", a, b], &dir);
        assert_eq!(gnu.status.code(), Some(expected), "GNU diff {a} {b}");
        assert_eq!(
            flick.status.code(),
            Some(expected),
            "flickzeug diff {a} {b}"
        );
    }
}

/// flickzeug-generated diffs must apply with GNU patch, and GNU-generated
/// diffs with flickzeug apply — both reproducing the target file exactly.
#[test]
fn cross_apply_with_gnu_patch() {
    if !have("diff") || !have("patch") {
        return;
    }

    for (name, old, new) in curated_pairs() {
        // Layout: old/f.txt and new/f.txt, so `-p1` strips the tree name and
        // both tools patch f.txt in a work tree.
        let dir = scratch_dir("cross-apply");
        fs::create_dir_all(dir.join("old")).unwrap();
        fs::create_dir_all(dir.join("new")).unwrap();
        fs::create_dir_all(dir.join("tree-gnu")).unwrap();
        fs::create_dir_all(dir.join("tree-flick")).unwrap();
        fs::write(dir.join("old/f.txt"), old).unwrap();
        fs::write(dir.join("new/f.txt"), new).unwrap();
        fs::write(dir.join("tree-gnu/f.txt"), old).unwrap();
        fs::write(dir.join("tree-flick/f.txt"), old).unwrap();

        // Direction 1: flickzeug diff -> GNU patch
        let flick_diff = flickzeug(&["diff", "old/f.txt", "new/f.txt"], &dir);
        fs::write(dir.join("flick.patch"), &flick_diff.stdout).unwrap();

        let gnu_patch = run(Command::new("patch")
            .args(["--batch", "-p1", "-i", "../flick.patch"])
            .current_dir(dir.join("tree-gnu")));
        assert!(
            gnu_patch.status.success(),
            "case {name}: GNU patch rejected flickzeug diff:\n{}{}",
            stdout_string(&gnu_patch),
            String::from_utf8_lossy(&gnu_patch.stderr),
        );
        assert_eq!(
            fs::read_to_string(dir.join("tree-gnu/f.txt")).unwrap(),
            new,
            "case {name}: GNU patch applying a flickzeug diff produced the wrong content"
        );

        // Direction 2: GNU diff -> flickzeug apply
        let gnu_diff = run(Command::new("diff")
            .args(["-u", "old/f.txt", "new/f.txt"])
            .current_dir(&dir));
        fs::write(dir.join("gnu.patch"), &gnu_diff.stdout).unwrap();

        let flick_apply = flickzeug(
            &["apply", "--strip", "1", "../gnu.patch"],
            &dir.join("tree-flick"),
        );
        assert_eq!(
            flick_apply.status.code(),
            Some(0),
            "case {name}: flickzeug apply rejected GNU diff:\n{}{}",
            stdout_string(&flick_apply),
            String::from_utf8_lossy(&flick_apply.stderr),
        );
        assert_eq!(
            fs::read_to_string(dir.join("tree-flick/f.txt")).unwrap_or_default(),
            new,
            "case {name}: flickzeug apply of a GNU diff produced the wrong content"
        );
    }
}

/// A patch whose line numbers have drifted must land in the same place with
/// both tools.
#[test]
fn apply_with_offset_matches_gnu_patch() {
    if !have("diff") || !have("patch") {
        return;
    }

    let dir = scratch_dir("offset");
    let v1 = "h1\nh2\nh3\nA\nB\nC\nx\ny\nz\n";
    let v2 = "h1\nh2\nh3\nA\nB-changed\nC\nx\ny\nz\n";
    // The same file with two lines inserted at the top: hunk offsets are stale.
    let drifted = "new0\nnew1\nh1\nh2\nh3\nA\nB\nC\nx\ny\nz\n";

    fs::write(dir.join("v1.txt"), v1).unwrap();
    fs::write(dir.join("v2.txt"), v2).unwrap();
    let diff_out = run(Command::new("diff")
        .args(["-u", "v1.txt", "v2.txt"])
        .current_dir(&dir));
    fs::write(dir.join("change.patch"), &diff_out.stdout).unwrap();

    fs::create_dir_all(dir.join("tree-gnu")).unwrap();
    fs::create_dir_all(dir.join("tree-flick")).unwrap();
    fs::write(dir.join("tree-gnu/v1.txt"), drifted).unwrap();
    fs::write(dir.join("tree-flick/v1.txt"), drifted).unwrap();

    let gnu = run(Command::new("patch")
        .args(["--batch", "-p0", "-i", "../change.patch"])
        .current_dir(dir.join("tree-gnu")));
    assert!(gnu.status.success());

    // GNU patch writes to the *old* name with -p0; flickzeug uses the new
    // name, which is the same file here after the v2->v1 name check. The
    // patch has old=v1.txt/new=v2.txt, so tell flickzeug to patch v1.txt by
    // copying the result: simplest is to also have v2.txt present.
    fs::copy(dir.join("tree-flick/v1.txt"), dir.join("tree-flick/v2.txt")).unwrap();
    let flick = flickzeug(&["apply", "../change.patch"], &dir.join("tree-flick"));
    assert_eq!(flick.status.code(), Some(0), "{flick:?}");

    assert_eq!(
        fs::read_to_string(dir.join("tree-gnu/v1.txt")).unwrap(),
        fs::read_to_string(dir.join("tree-flick/v2.txt")).unwrap(),
        "offset application differs from GNU patch"
    );
}

/// Reverse application must agree with `patch -R`.
#[test]
fn reverse_apply_matches_gnu_patch() {
    if !have("diff") || !have("patch") {
        return;
    }

    let dir = scratch_dir("reverse");
    let old = "a\nb\nc\n";
    let new = "a\nB\nc\nd\n";
    fs::write(dir.join("f.txt"), old).unwrap();
    fs::write(dir.join("g.txt"), new).unwrap();
    let diff_out = run(Command::new("diff")
        .args(["-u", "f.txt", "g.txt"])
        .current_dir(&dir));
    fs::write(dir.join("p.patch"), &diff_out.stdout).unwrap();

    fs::create_dir_all(dir.join("tree-gnu")).unwrap();
    fs::create_dir_all(dir.join("tree-flick")).unwrap();
    fs::write(dir.join("tree-gnu/f.txt"), new).unwrap();
    fs::write(dir.join("tree-flick/f.txt"), new).unwrap();
    fs::write(dir.join("tree-flick/g.txt"), new).unwrap();

    let gnu = run(Command::new("patch")
        .args(["--batch", "-R", "-p0", "-i", "../p.patch"])
        .current_dir(dir.join("tree-gnu")));
    assert!(gnu.status.success());
    assert_eq!(fs::read_to_string(dir.join("tree-gnu/f.txt")).unwrap(), old);

    let flick = flickzeug(
        &["apply", "--reverse", "../p.patch"],
        &dir.join("tree-flick"),
    );
    assert_eq!(flick.status.code(), Some(0), "{flick:?}");
    // Reversed, the "old" side is g.txt -> flickzeug writes f.txt
    assert_eq!(
        fs::read_to_string(dir.join("tree-flick/f.txt")).unwrap(),
        old,
        "reverse application differs from GNU patch"
    );
}

/// A patch that does not apply must fail with exit code 1 in both tools.
#[test]
fn failed_apply_exit_codes_match_gnu_patch() {
    if !have("patch") {
        return;
    }

    let patch = "\
--- f.txt
+++ f.txt
@@ -1,3 +1,3 @@
 context before
-old line
+new line
 context after
";

    let dir = scratch_dir("fail");
    fs::write(dir.join("f.txt"), "completely unrelated content\n").unwrap();
    fs::write(dir.join("p.patch"), patch).unwrap();

    let gnu = run(Command::new("patch")
        .args(["--batch", "-p0", "-i", "p.patch"])
        .current_dir(&dir));
    assert_eq!(gnu.status.code(), Some(1), "GNU patch should exit 1");

    fs::write(dir.join("f.txt"), "completely unrelated content\n").unwrap();
    let flick = flickzeug(&["apply", "p.patch"], &dir);
    assert_eq!(flick.status.code(), Some(1), "flickzeug should exit 1");
}

/// flickzeug diffs must be accepted by `git apply`, and `git diff` output by
/// `flickzeug apply`.
#[test]
fn cross_apply_with_git() {
    if !have("git") {
        return;
    }

    let old = "line one\nline two\nline three\nline four\n";
    let new = "line one\nline 2\nline three\nline four\nline five\n";

    let dir = scratch_dir("git");
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = run(Command::new("git")
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .current_dir(&repo));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };

    git(&["init", "-q"]);
    fs::write(repo.join("f.txt"), old).unwrap();
    git(&["add", "f.txt"]);
    git(&["commit", "-qm", "base"]);

    // Direction 1: git diff -> flickzeug apply
    fs::write(repo.join("f.txt"), new).unwrap();
    let git_diff = git(&["diff"]);
    fs::write(dir.join("git.patch"), &git_diff.stdout).unwrap();
    git(&["checkout", "-q", "--", "f.txt"]); // back to old

    let flick = flickzeug(&["apply", "../git.patch"], &repo);
    assert_eq!(
        flick.status.code(),
        Some(0),
        "flickzeug apply rejected git diff output: {flick:?}"
    );
    assert_eq!(fs::read_to_string(repo.join("f.txt")).unwrap(), new);
    git(&["checkout", "-q", "--", "f.txt"]);

    // Direction 2: flickzeug diff -> git apply. Give the diff a/ b/ prefixed
    // names the way git likes them.
    fs::write(dir.join("old-copy.txt"), old).unwrap();
    fs::write(dir.join("new-copy.txt"), new).unwrap();
    let flick_diff = flickzeug(&["diff", "old-copy.txt", "new-copy.txt"], &dir);
    let patch_text = stdout_string(&flick_diff)
        .replace("--- old-copy.txt", "--- a/f.txt")
        .replace("+++ new-copy.txt", "+++ b/f.txt");
    fs::write(dir.join("flick.patch"), patch_text).unwrap();

    let git_apply = run(Command::new("git")
        .args(["apply", "../flick.patch"])
        .current_dir(&repo));
    assert!(
        git_apply.status.success(),
        "git apply rejected flickzeug diff: {}",
        String::from_utf8_lossy(&git_apply.stderr)
    );
    assert_eq!(fs::read_to_string(repo.join("f.txt")).unwrap(), new);
}

/// Merge output must be byte-identical to `git merge-file` for both conflict
/// styles, including exit codes.
#[test]
fn merge_matches_git_merge_file() {
    if !have("git") {
        return;
    }

    let cases = [
        (
            "clean",
            "a\nb\nc\nd\ne\n", // base
            "A\nb\nc\nd\ne\n", // ours
            "a\nb\nc\nd\nE\n", // theirs
            false,
        ),
        (
            "conflict",
            "a\nb\nc\nd\ne\n",
            "OURS\nb\nc\nd\ne\n",
            "THEIRS\nb\nc\nd\ne\n",
            true,
        ),
        (
            "both-same-change",
            "a\nb\nc\n",
            "a\nBOTH\nc\n",
            "a\nBOTH\nc\n",
            false,
        ),
    ];

    for (name, base, ours, theirs, conflicts) in cases {
        let dir = scratch_dir("merge");
        fs::write(dir.join("base.txt"), base).unwrap();
        fs::write(dir.join("ours.txt"), ours).unwrap();
        fs::write(dir.join("theirs.txt"), theirs).unwrap();

        for style in ["merge", "diff3"] {
            let mut git_cmd = Command::new("git");
            git_cmd.args(["merge-file", "--stdout"]);
            if style == "diff3" {
                git_cmd.arg("--diff3");
            }
            // flickzeug's default labels
            git_cmd.args(["-L", "ours", "-L", "original", "-L", "theirs"]);
            git_cmd.args(["ours.txt", "base.txt", "theirs.txt"]);
            let git = run(git_cmd.current_dir(&dir));

            let flick = flickzeug(
                &[
                    "merge",
                    "ours.txt",
                    "base.txt",
                    "theirs.txt",
                    "--style",
                    style,
                ],
                &dir,
            );

            assert_eq!(
                stdout_string(&git),
                stdout_string(&flick),
                "case {name} ({style}): merge output differs from git merge-file"
            );
            // git merge-file exits with the number of conflicts; flickzeug
            // with 1 if there are any.
            assert_eq!(
                git.status.code().is_some_and(|c| c > 0),
                conflicts,
                "case {name}: unexpected git conflict state"
            );
            assert_eq!(
                flick.status.code(),
                Some(i32::from(conflicts)),
                "case {name} ({style}): flickzeug exit code"
            );
        }
    }
}
