//! Benchmarks for the hot paths: diffing, parsing, applying (exact, offset
//! and fuzzy), and merging.
//!
//! Run with `cargo bench`. The inputs are deterministic so results are
//! comparable across runs and machines.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use flickzeug::{
    ApplyConfig, Diff, FuzzyConfig, HunkRangeStrategy, ParserConfig, apply_bytes,
    apply_bytes_with_config, create_patch, merge, patch_from_str_with_config,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
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

/// A synthetic source file of `lines` lines with some repetition (so diffs
/// have real common subsequences, like code does).
fn synthetic_file(rng: &mut Rng, lines: usize) -> String {
    let vocabulary = [
        "fn process(input: &str) -> Result<Output, Error> {",
        "}",
        "",
        "    let value = input.parse()?;",
        "    // fall through to the slow path",
        "    if value > threshold {",
        "        return Err(Error::TooLarge);",
        "    }",
        "use std::collections::HashMap;",
        "#[derive(Debug, Clone)]",
    ];
    let mut text = String::new();
    for i in 0..lines {
        if rng.below(4) == 0 {
            text.push_str(&format!("    let unique_{i} = compute({i});"));
        } else {
            text.push_str(vocabulary[rng.below(vocabulary.len())]);
        }
        text.push('\n');
    }
    text
}

/// Mutate ~5% of the lines, scattered through the file.
fn mutate(rng: &mut Rng, text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        match rng.below(20) {
            0 => out.push_str(&format!("    let changed_{i} = 1;\n")),
            1 => {} // drop
            _ => out.push_str(line),
        }
    }
    out
}

/// All the real-world patches from the parser test corpus, concatenated.
fn corpus() -> String {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/patch/test-data");
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "patch"))
        .collect();
    entries.sort();
    entries
        .into_iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect()
}

fn benchmarks(c: &mut Criterion) {
    let mut rng = Rng(0xBE7C_0001);
    let old = synthetic_file(&mut rng, 5_000);
    let new = mutate(&mut rng, &old);

    // Diff two ~5k line files
    c.bench_function("create_patch/5k-lines", |b| {
        b.iter(|| black_box(create_patch(black_box(&old), black_box(&new))))
    });

    // Parse the real-world conda-forge patch corpus (~40 patches)
    let corpus = corpus();
    let lenient = ParserConfig {
        hunk_strategy: HunkRangeStrategy::Recount,
        skip_order_check: true,
        ..ParserConfig::default()
    };
    c.bench_function("parse/corpus", |b| {
        b.iter(|| {
            black_box(patch_from_str_with_config(
                black_box(&corpus),
                lenient.clone(),
            ))
        })
    });

    // Apply a clean patch (all hunks match exactly at their stated positions)
    let patch = create_patch(&old, &new);
    let patch_text = patch.to_string();
    let diff = Diff::from_bytes(patch_text.as_bytes()).unwrap();
    c.bench_function("apply/exact", |b| {
        b.iter(|| black_box(apply_bytes(black_box(old.as_bytes()), black_box(&diff))))
    });

    // Apply the same patch to a file with 50 extra lines at the top: every
    // hunk needs the interleaved offset search
    let drifted = format!("{}{}", "// drifted line\n".repeat(50), old);
    c.bench_function("apply/offset", |b| {
        b.iter(|| {
            black_box(apply_bytes(
                black_box(drifted.as_bytes()),
                black_box(&diff),
            ))
        })
    });

    // Worst case: a patch that cannot apply, forcing a full scan at every
    // fuzz level (the rattler-build fuzzy configuration)
    let bogus_patch = "\
--- a/f
+++ b/f
@@ -2400,7 +2400,7 @@
 no such context anywhere
 in the target file at all
 (three unmatched context lines)
-and a deleted line that is absent
+and its replacement
 more context that will not match
 still not matching
 nothing here either
";
    let bogus = Diff::from_bytes(bogus_patch.as_bytes()).unwrap();
    let fuzzy = ApplyConfig {
        fuzzy_config: FuzzyConfig {
            max_fuzz: 2,
            ignore_whitespace: true,
            ..FuzzyConfig::default()
        },
        ..ApplyConfig::default()
    };
    c.bench_function("apply/fuzzy-miss", |b| {
        b.iter(|| {
            black_box(apply_bytes_with_config(
                black_box(old.as_bytes()),
                black_box(&bogus),
                black_box(&fuzzy),
            ))
        })
    });

    // Three-way merge with both sides changed (no conflicts)
    let ours = mutate(&mut rng, &old);
    c.bench_function("merge/5k-lines", |b| {
        b.iter(|| {
            black_box(merge(
                black_box(&old),
                black_box(&ours),
                black_box(&new),
            ))
        })
    });
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
