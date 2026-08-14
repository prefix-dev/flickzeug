# flickzeug — deep review

*Reviewed at v0.5.4 (commit `8518b8d`), August 2026. Every finding in the
"verified bugs" section was reproduced against the library before writing it
down; the repro snippets are copy-pasteable.*

flickzeug is in good shape overall: the core Myers diff and diff3 merge are
solid inherited code, the fuzzy apply and already-applied detection are
genuinely valuable additions over upstream diffy, and the real-world patch
corpus in the test suite is a great asset. The findings below are ordered by
how much they matter, not by how easy they are to fix.

---

## 1. Verified bugs

### 1.1 `LineIter` misreads a `\r` that is not part of a CRLF — panic + silent data loss — **fixed in this branch**

`utils.rs` checked for a `\r` before the line end even when no `\n` was found:

```rust
// panicked (subtract with overflow):
flickzeug::Diff::from_str("\r");
flickzeug::create_patch("\r", "x");

// silently corrupted content: the final line "foo\rx" (no trailing newline)
// was truncated to "foo" plus a fabricated CRLF, so applying any patch to
// this file dropped the "x":
let (out, _) = apply("line1\nold\nfoo\rx", &diff)?;  // -> "line1\nnew\nfoo\n"
```

The panic is reachable from `Diff::from_str` / `Diff::from_bytes`, i.e. from
*untrusted patch input* in rattler-build. Fixed by only treating `\r` as part
of a line ending when it directly precedes a found `\n`; regression tests
added in `src/utils.rs`.

### 1.2 Function-context hunk headers do not roundtrip — **fixed in this branch**

Formatting a parsed hunk with a function context (`@@ -1,3 +1,3 @@ fn main() {`)
produced a **double space** after `@@` and additionally wrote the stored
header line ending before the closing `writeln!`, injecting a **stray blank
line** into the output:

```text
in:  @@ -1,3 +1,3 @@ fn main() {
out: @@ -1,3 +1,3 @@  fn main() {
                                     <- spurious empty line
```

Since the blank line parses as an empty context line, a formatted-then-reparsed
patch would even have shifted hunk contents. Any git-produced patch that is
parsed and re-emitted (error messages, `Diff::to_string`, `to_bytes`) was
affected. Fixed in `src/patch/format.rs` for both the `Display` and the
`io::Write` paths; roundtrip test added.

### 1.3 Applying a patch rewrites line endings of untouched lines — **open, needs a decision**

The default `LineEndHandling::EnsureFileLineEnding` computes the *most common*
line ending of the input and then rewrites **every** line of the output with
it, including lines no hunk ever touched:

```rust
let base = "keep\r\nold\r\nplain\nmore\r\n";      // 'plain' uses LF
// patch replaces only the 'old' line
let (out, _) = apply(base, &diff)?;
// out == "keep\r\nnew\r\nplain\r\nmore\r\n"      // 'plain' got CRLF!
```

GNU patch and `git apply` never modify lines outside of hunks. For a tool
whose consumers patch third-party sources (conda-forge feedstocks), silently
normalizing a mixed-endings file can produce noisy rebuilds and broken
checksums. The `// TODO: Keep line ending as is like it was before.` comments
in `apply.rs` suggest this is known.

**Recommendation:** add a `KeepOriginal` variant (each line keeps the ending
it had; inserted lines inherit the ending of the neighbouring context, falling
back to the hunk's dominant ending) and make it the default on the next
breaking release. The three existing "Ensure*" modes remain as opt-ins.

### 1.4 A malformed hunk mid-diff is silently swallowed — **open**

`parse::hunks()` treats *any* hunk parse error as "end of this file's hunks"
(the `// TODO: Handle properly` in `parse.rs:513`):

```rust
// second hunk header is malformed ("@@ -10,2 +10,2 XX broken header"):
let d = Diff::from_str(s).unwrap();      // Ok!
assert_eq!(d.hunks().len(), 1);          // second hunk silently dropped
```

A truncated patch then applies "successfully" while doing half the job — for a
patching pipeline that is arguably worse than the panic in 1.1. The swallow is
load-bearing (it is how `parse_multiple` finds the end of one file's hunks
before the next `diff --git`/`---` header), so the fix is to distinguish
"line that legitimately starts the next section" (`diff --git `, `--- `,
`Index: `, `From `, EOF …) from "line that parses as neither hunk content nor
a section start", and return an error for the latter. Related: the *bytes*
variant of `parse_multiple` breaks out of the loop on `(Err, Err)` where the
*str* variant returns the error — the two front ends should behave
identically.

### 1.5 README examples didn't compile — **fixed in this branch**

The apply example used `Patch::from_str` (does not exist — `Patch<T>` is
`Vec<Diff<T>>`) and ignored that `apply` now returns `(content, stats)`; the
suggested dependency version was still `0.4`. Fixed, and the new example is
verified by being an actual compiling doctest-style snippet.

---

## 2. Correctness & semantics concerns (not yet bugs, but worth deciding deliberately)

* **The fuzzy similarity threshold is a hard-coded 0.8** (`FuzzyComparable::fuzzy_eq`).
  Whenever fuzz level ≥ 1 kicks in, *every non-ignored context line — and
  every deleted line —* only needs 80% Levenshtein similarity to "match".
  GNU patch never does this: fuzz only *ignores* edge context lines, all other
  lines must match exactly (modulo `--ignore-whitespace`). An 80% match on a
  deleted line means flickzeug can delete a line that differs from what the
  patch says it deletes. Recommend: make the threshold a `FuzzyConfig` field,
  default it to `1.0` (exact) for delete lines, and document the trade-off.

* **Hunks with fewer context lines than the fuzz level fall back to
  whole-hunk fuzzy matching** (`match_fragment_fuzzy`, the
  `pre_image_context_indices.len() < fuzz_level` branch). A context-free hunk
  can then be placed by 0.8-similarity of its *deleted* lines alone. GNU patch
  refuses to fuzz hunks without context.

* **`HunkRangeStrategy::Recount` still trusts the header counts while
  *reading* hunk lines** (`hunk_lines` stops at `expected_old/new_lines`).
  If a header undercounts, the extra lines are abandoned and — via 1.4 —
  silently dropped; if it overcounts, the hunk errors out and is dropped
  entirely. True `git apply --recount` semantics require reading by content
  (prefix classification) first and computing ranges after. Worth fixing
  together with 1.4.

* **`is_diff_applied_with_config` compares with `similarity(...) >= 1.0`**
  (`apply.rs:484`) — a float-equality idiom that works but computes a full
  Levenshtein distance only to test exact equality. A dedicated
  `normalized_eq` would be clearer and much faster (see §3).

* **`LineEnd::choose_from_scores` ties break on `cfg!(windows)`** — the same
  input produces different output per host OS, and there is no Windows CI to
  observe it (see §5). Platform-dependent output from a pure text function is
  surprising; consider tying to the patch/file instead, or documenting it
  loudly.

---

## 3. Performance

The fuzzy path is algorithmically fine (interleaved outward search like GNU
patch) but does a lot of avoidable per-candidate-position work. For a hunk
that doesn't match (worst case), for **each** of the O(file) candidate
positions and **each** fuzz level, `match_fragment_fuzzy` re-allocates:

* `pre_image_lines`, `image_lines`, `context_indices`,
  `pre_image_context_indices` (four `Vec`s), and
* `generate_fuzz_combinations(...)` — identical for every position!

and `FuzzyComparable::similarity` allocates **two `String`s per line
comparison** (`to_string()`/`to_lowercase()` even when no normalization is
configured) before running O(len²) Levenshtein. Cheap wins, roughly in order:

1. Hoist the per-hunk data (pre-image, context indices, fuzz combinations)
   out of the position loop — pure refactor, no behavior change.
2. In `similarity`, borrow when `ignore_case`/`ignore_whitespace` are off, and
   short-circuit equality without Levenshtein (also fixes the
   `is_diff_applied` case above).
3. Length-based early exit: if `|len(a) - len(b)| / max > 1 - threshold`,
   similarity can never reach the threshold — skip Levenshtein.
4. `utils::find_byte` is a naive scan with an `// XXX Maybe use memchr?`
   comment — take the hint; line splitting is the hottest loop in the crate
   and `memchr` is SIMD-accelerated with no MSRV concerns.
5. Also low-hanging: `apply_hunk_preserving_context` does O(n) `Vec::remove`/
   `insert` per changed line; `generate_fuzz_combinations` emits duplicates
   (the empty set at every level, lower levels re-tested at higher ones).

None of this is measured — which is the actual gap. **Add a criterion bench**
(apply a large conda-forge patch series; diff two large files) so these
changes and future regressions are visible. `hyperfine` against GNU
patch/diff/git would make a nice README badge, too.

---

## 4. API & design

* **`Patch<'a, T> = Vec<Diff<'a, T>>` is a footgun.** A bare type alias has no
  methods (see the README bug it caused), can't get a `Display`, and inverts
  upstream diffy's naming (`diffy::Patch` ≈ `flickzeug::Diff`), which will
  bite anyone migrating. Recommend a `Patch` newtype with `from_str`/
  `from_bytes`/`Display`/`iter()`, and `apply_all(&mut FileSet)`-style helpers
  — this is exactly the logic the new CLI had to hand-roll (path resolution,
  create/delete/rename), and it belongs in the library.

* **The str/bytes API split is heavily duplicated.** `apply_with_config` /
  `apply_bytes_with_config` are byte-identical twins (including the two
  line-ending scoring blocks), the four `parse*` front ends duplicate the
  multi-diff loop with *divergent* error handling (§1.4), and `format.rs`
  duplicates every writer as `Display` + `write_into`. One generic
  `T: Text + FuzzyComparable + ?Sized` implementation with thin `str`/`[u8]`
  wrappers would remove several hundred lines and make the pairs impossible to
  desynchronize. Also missing for parity: `apply_reporting` /
  `is_diff_applied_with_config` for `str` (they exist only for bytes).

* **Errors are stringly and unlocated.** `ApplyError(usize, String)` has
  private fields (callers can't get the hunk index without parsing the
  message) and a multi-line `Display`, which composes badly when wrapped by
  other errors. `ParsePatchError` carries no line number or offending line —
  painful when a 2000-line feedstock patch fails to parse. Both are cheap to
  fix pre-1.0: accessor methods + a `line: usize` field.

* **Git metadata is parsed but thrown away.** The parser understands
  `diff --git`, `rename from/to`, `deleted/new file mode`, yet `Diff` only
  exposes two filenames. Consumers must re-derive "is this a create/delete/
  rename?" from `original().is_none()` heuristics. Expose a small
  `FileMetadata { kind: Create|Delete|Rename|Modify, old_mode, new_mode, … }`.
  (Binary patches / `GIT binary patch` sections are silently skipped today —
  at minimum they deserve a documented error.)

* **`nu-ansi-term` should be optional.** It exists only for
  `PatchFormatter::with_color`; rattler-build-style consumers pay the
  dependency for nothing. `color = ["dep:nu-ansi-term"]` (default-on for
  compatibility) is a one-evening change.

* **Merge conflict labels are hard-coded** (`ours` / `original` / `theirs`),
  and conflict markers are always `\n` even in a CRLF file. `git merge-file
  -L` style labels are a small, obviously useful addition to `MergeOptions`;
  return a typed `MergeConflicts { output, conflict_count }` instead of
  `Err(String)` while touching it.

* Smaller items: `FuzzyConfig` and `ApplyConfig` should be `#[non_exhaustive]`
  (they'll grow); `lib.rs` still links `struct.Patch.html` (stale pre-rename
  anchors — switch to intra-doc links, docs.rs currently 404s these);
  `DiffLine`/`DiffOptions::diff()` are `#[allow(dead_code)]` — either expose a
  line-level diff API (useful!) or delete; `Hunk::lines()` returning
  `(&T, Option<LineEnd>)` tuples everywhere would read better as a small
  `LineContent<T>` struct.

---

## 5. Testing & tooling gaps

* **Fuzzing.** Finding 1.1 (a panic on `"\r"` reachable from untrusted input)
  is precisely what a 30-second `cargo-fuzz` run would have caught. Add two
  targets: `parse_bytes(data)` must never panic, and
  `apply(base, parse(patch))` must never panic. Wire into CI weekly.

* **Property/roundtrip tests.** Three invariants make excellent proptests:
  `parse(format(p)) == p` (1.2 violated this), `apply(a, create_patch(a, b))
  == b` (exists only as scattered examples), and
  `merge(o, a, o) == Ok(a)`.

* **Windows + macOS CI.** All of `line_end.rs` exists because of platform
  line endings, `choose_from_scores` literally branches on `cfg!(windows)` —
  and CI runs on `ubuntu-latest` only. Add a matrix. (This branch already
  extends CI to `--all-features` for the new CLI.)

* **MSRV job runs `cargo check` only** — MSRV-incompatible *test* code or
  doctests slip through. Use `cargo test --no-run` there, and consider
  `cargo-semver-checks` in PR CI rather than only at release time
  (release-plz's `semver_check` is good but late).

* The `#[test]` corpus is strong on parsing, thinner on apply edge cases:
  multi-hunk fuzz interactions, `EnsureLineEnding` variants, CRLF patches
  against LF files (§1.3 would have shown up), and `apply` against files
  without trailing newline (1.1 would have).

---

## 6. Standalone binaries (diff / patch / merge) — done as `cli` feature

This branch adds the binaries — see `src/main.rs`, gated behind the `cli`
feature so the library's dependency tree is untouched:

```console
cargo install flickzeug --features cli
flickzeug diff old.txt new.txt          # exit 0 same / 1 differ / 2 trouble
flickzeug apply series.patch -d src -F2 # fuzzy apply, already-applied → skip
flickzeug merge ours base theirs        # git merge-file argument order
```

Design notes and where to take it:

* **Why one multi-tool binary** instead of `fz-diff`/`fz-patch`/`fz-merge`:
  one crate target, one `--help` tree, trivially extended with `flickzeug
  interdiff`, `flickzeug recount`, etc. If drop-in names are ever wanted,
  ship hardlinks/shims that dispatch on `argv[0]` (busybox style).
* **`apply` showcases the library's differentiators**: fuzzy matching
  (`-F/--fuzz`), *already-applied detection* via `apply_bytes_reporting`
  (GNU patch can only offer the interactive "assume -R?" prompt here), and
  lenient parsing (`--lenient` = `Recount` + `skip_order_check`) for the
  malformed-but-salvageable patches conda-forge is full of. It also refuses
  paths escaping the target directory (GNU patch CVE-2020-14855 class).
* **What it deliberately does not do yet**: `-p` counts *after* the automatic
  `a/ b/` strip (differs from GNU `-pN`, documented in `--help`); no backup
  files (`--backup`), no `.rej` emission for failed hunks (today: report +
  exit 1), no binary patches. `.rej` output is the most valuable follow-up,
  and wants the `Patch` newtype from §4 (format a `Diff` containing only the
  failed hunks).
* **Distribution**: once this is deemed useful, publish prebuilt binaries via
  `cargo-dist` (or a small release workflow) with `x86_64/aarch64` ×
  `linux-musl (static)/macos/windows` targets, and consider a separate
  `flickzeug-cli` crate in a workspace if the clap dependency should not
  appear in the library's `[dependencies]` at all (optional deps still affect
  `cargo vet`/audit surface for some consumers).

---

## 7. Suggested roadmap

| Priority | Item | Size |
|---|---|---|
| P0 | ~~`LineIter` CR panic + data loss~~ (fixed here) | S |
| P0 | ~~Function-context roundtrip~~ (fixed here) | S |
| P0 | Stop silently dropping malformed hunks (§1.4) + align str/bytes front ends | M |
| P1 | Line-ending preservation for untouched lines, new default (§1.3) | M |
| P1 | Fuzzing targets + proptest roundtrips (§5) | S |
| P1 | Windows/macOS CI matrix; MSRV job compiles tests | S |
| P1 | Hoist per-hunk work out of the fuzzy position loop; borrow in `similarity`; memchr (§3) | M |
| P2 | Configurable similarity threshold; exact match for delete lines (§2) | S |
| P2 | `Patch` newtype + file-set apply helpers; `.rej` output for the CLI (§4, §6) | M |
| P2 | Structured errors with positions; expose git metadata; optional `nu-ansi-term` | M |
| P3 | Merge labels + typed conflict result; criterion benches; true `--recount` | M |

*(S ≈ an evening, M ≈ a day or two.)*
