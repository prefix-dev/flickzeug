//! Randomized (but deterministic) invariant tests.
//!
//! Three properties that must hold for arbitrary inputs:
//!
//! 1. diff/apply roundtrip: `apply(a, create_patch(a, b)) == b`
//! 2. format/parse roundtrip: `parse(format(patch)) == patch`
//! 3. trivial merges: `merge(o, a, o) == Ok(a)` and `merge(o, o, b) == Ok(b)`
//!
//! The generator is a tiny xorshift PRNG with fixed seeds so failures are
//! reproducible; the interesting edge cases (missing trailing newline, CRLF
//! and mixed endings, empty files, empty lines) are drawn deliberately often.

use flickzeug::{Diff, apply, apply_bytes, create_patch, create_patch_bytes, merge};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Generate a random text: lines drawn from a small vocabulary (so diffs have
/// real common subsequences), mixed line endings, sometimes no trailing
/// newline, sometimes empty.
fn random_text(rng: &mut Rng) -> String {
    let vocabulary = [
        "fn main() {",
        "}",
        "",
        "    println!(\"hello\");",
        "let x = 42;",
        "// comment",
        "use std::fmt;",
        "    return 0;",
        "#[derive(Debug)]",
        "world",
    ];

    let line_count = rng.below(30);
    let mut text = String::new();
    for i in 0..line_count {
        text.push_str(vocabulary[rng.below(vocabulary.len())]);
        let last = i + 1 == line_count;
        if last && rng.below(4) == 0 {
            break; // no trailing newline
        }
        text.push_str(if rng.below(3) == 0 { "\r\n" } else { "\n" });
    }
    text
}

/// Mutate `text` into a related text: keep most lines, change/drop/add a few.
fn mutate_text(rng: &mut Rng, text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::new();
    for line in &lines {
        match rng.below(8) {
            0 => {} // drop the line
            1 => {
                out.push_str("inserted line\n");
                out.push_str(line);
            }
            2 => out.push_str("changed line\n"),
            _ => out.push_str(line),
        }
    }
    if rng.below(6) == 0 {
        out.push_str("trailing addition");
        if rng.below(2) == 0 {
            out.push('\n');
        }
    }
    out
}

#[test]
fn diff_apply_roundtrip_str() {
    let mut rng = Rng(0x5EED_0001);
    for case in 0..500 {
        let a = random_text(&mut rng);
        let b = mutate_text(&mut rng, &a);

        let patch = create_patch(&a, &b);
        let (applied, _stats) = apply(&a, &patch)
            .unwrap_or_else(|err| panic!("case {case}: apply failed\na={a:?}\nb={b:?}\n{err}"));
        assert_eq!(
            applied, b,
            "case {case}: roundtrip mismatch\na={a:?}\nb={b:?}\npatch={patch}"
        );
    }
}

#[test]
fn diff_apply_roundtrip_bytes() {
    let mut rng = Rng(0x5EED_0002);
    for case in 0..500 {
        let a = random_text(&mut rng);
        let b = mutate_text(&mut rng, &a);

        let patch = create_patch_bytes(a.as_bytes(), b.as_bytes());
        let (applied, _stats) = apply_bytes(a.as_bytes(), &patch)
            .unwrap_or_else(|err| panic!("case {case}: apply failed\na={a:?}\nb={b:?}\n{err}"));
        assert_eq!(
            applied,
            b.as_bytes(),
            "case {case}: roundtrip mismatch\na={a:?}\nb={b:?}"
        );
    }
}

#[test]
fn format_parse_roundtrip() {
    let mut rng = Rng(0x5EED_0003);
    for case in 0..500 {
        let a = random_text(&mut rng);
        let b = mutate_text(&mut rng, &a);

        let patch = create_patch(&a, &b);
        let formatted = patch.to_string();
        let reparsed = Diff::from_str(&formatted).unwrap_or_else(|err| {
            panic!("case {case}: reparse failed\npatch={formatted:?}\n{err}")
        });
        assert_eq!(
            formatted,
            reparsed.to_string(),
            "case {case}: formatting is not stable across parse"
        );
        assert_eq!(
            patch.hunks(),
            reparsed.hunks(),
            "case {case}: hunks changed across format/parse\npatch={formatted:?}"
        );
    }
}

#[test]
fn trivial_merges() {
    let mut rng = Rng(0x5EED_0004);
    for case in 0..300 {
        let o = random_text(&mut rng);
        let a = mutate_text(&mut rng, &o);

        // Only one side changed: must merge cleanly to that side.
        assert_eq!(
            merge(&o, &a, &o),
            Ok(a.clone()),
            "case {case}: merge(o, a, o) != a\no={o:?}\na={a:?}"
        );
        assert_eq!(
            merge(&o, &o, &a),
            Ok(a.clone()),
            "case {case}: merge(o, o, a) != a\no={o:?}\na={a:?}"
        );
        // Nothing changed at all.
        assert_eq!(merge(&o, &o, &o), Ok(o.clone()), "case {case}");
    }
}
