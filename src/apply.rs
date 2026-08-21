use crate::{
    line_end::LineEnd,
    patch::{Diff, Hunk, Line},
    utils::{LineIter, Text},
};
use std::{borrow::Cow, fmt, iter};

/// An error returned when [`apply`]ing a `Patch` fails
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApplyError(usize, String);

impl ApplyError {
    /// The 1-based index of the hunk that could not be applied
    pub fn hunk_index(&self) -> usize {
        self.0
    }

    /// The formatted content of the hunk that could not be applied
    pub fn hunk_content(&self) -> &str {
        &self.1
    }
}

impl fmt::Debug for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApplyError")
            .field(&self.0)
            .field(&self.1)
            .finish()
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "error applying hunk #{}: could not find context in target file",
            self.0
        )?;
        writeln!(f)?;
        writeln!(f, "Hunk content:")?;
        write!(f, "{}", self.1)
    }
}

impl std::error::Error for ApplyError {}

/// Statistics for a single hunk application
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HunkStats {
    /// Number of lines added in this hunk
    added: usize,
    /// Number of lines deleted in this hunk
    deleted: usize,
    /// Number of context lines in this hunk
    context: usize,
}

/// Statistics about the changes made when applying a patch
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyStats {
    /// Total number of lines added
    pub lines_added: usize,
    /// Total number of lines deleted
    pub lines_deleted: usize,
    /// Total number of context lines (unchanged)
    pub lines_context: usize,
    /// Number of hunks successfully applied
    pub hunks_applied: usize,
}

impl ApplyStats {
    /// Create new empty statistics
    fn new() -> Self {
        Self {
            lines_added: 0,
            lines_deleted: 0,
            lines_context: 0,
            hunks_applied: 0,
        }
    }

    /// Add statistics from a hunk
    fn add_hunk(&mut self, hunk_stats: HunkStats) {
        self.lines_added += hunk_stats.added;
        self.lines_deleted += hunk_stats.deleted;
        self.lines_context += hunk_stats.context;
        self.hunks_applied += 1;
    }

    /// Returns whether any changes were made
    pub fn has_changes(&self) -> bool {
        self.lines_added > 0 || self.lines_deleted > 0
    }
}

/// Result of applying a patch with statistics
///
/// # Examples
///
/// ```
/// use flickzeug::{apply, Diff};
///
/// let base = "line 1\nline 2\n";
/// let patch_str = "--- a\n+++ b\n@@ -1,2 +1,2 @@\n line 1\n-line 2\n+line 2 modified\n";
/// let diff = Diff::from_str(patch_str).unwrap();
///
/// let (content, stats) = apply(base, &diff).unwrap();
/// assert_eq!(content, "line 1\nline 2 modified\n");
/// assert!(stats.has_changes());
/// ```
pub type ApplyResult<T, E = ApplyError> = Result<(T, ApplyStats), E>;

/// Configuration for patch application
#[derive(Default, Debug, Clone)]
pub struct ApplyConfig {
    /// Configuration of line end handling
    pub line_end_strategy: LineEndHandling,
    /// Configuration of fuzzy matching
    pub fuzzy_config: FuzzyConfig,
}

/// Configuration of line end handling
#[derive(Debug, Clone, Default)]
pub enum LineEndHandling {
    /// Keep every line ending as it is in the original file (default).
    ///
    /// Lines the patch does not touch are copied byte for byte, like GNU
    /// patch does. Inserted lines take the line ending of the closest
    /// retained line in the file (looking backwards first, then forwards),
    /// so that a patch written with `\n` endings inserts `\r\n` lines into a
    /// `\r\n` file and vice versa. An inserted line that the patch marks as
    /// having no line ending ("\ No newline at end of file") keeps that.
    #[default]
    KeepOriginal,
    /// Replace every line ending with the dominant line ending of the patch.
    ///
    /// Note that this rewrites the endings of *all* lines in the file, even
    /// lines no hunk touches.
    EnsurePatchLineEnding,
    /// Replace every line ending with the dominant line ending of the
    /// original file.
    ///
    /// Note that this rewrites the endings of *all* lines in the file: a
    /// file with mixed line endings is normalized to its most common one.
    EnsureFileLineEnding,
    /// Enforce one specific line ending for the entire output file.
    EnsureLineEnding(LineEnd),
}

/// Configuration for fuzzy matching behavior
#[derive(Debug, Clone)]
pub struct FuzzyConfig {
    /// Maximum number of context lines that can be ignored (fuzz factor)
    pub max_fuzz: usize,
    /// Whether to allow whitespace-only differences in context lines
    pub ignore_whitespace: bool,
    /// Whether to perform case-insensitive matching
    pub ignore_case: bool,
    /// Minimum similarity (`0.0..=1.0`) a non-ignored *context* line must
    /// have to count as matching when fuzzy matching is active. `1.0`
    /// requires full equality (modulo `ignore_whitespace`/`ignore_case`),
    /// which matches GNU patch behavior. *Deleted* lines always require full
    /// (normalized) equality regardless of this setting — a patch must never
    /// delete a line other than the one it names.
    pub similarity_threshold: f32,
}

impl Default for FuzzyConfig {
    fn default() -> Self {
        Self {
            max_fuzz: 2,
            ignore_whitespace: false,
            ignore_case: false,
            similarity_threshold: 0.8,
        }
    }
}

/// Trait for types that can be compared with fuzzy matching
pub trait FuzzyComparable {
    /// Similarity-based equality using
    /// [`FuzzyConfig::similarity_threshold`].
    fn fuzzy_eq(&self, other: &Self, config: &ApplyConfig) -> bool;

    /// Similarity in `0.0..=1.0` (Levenshtein-based), after applying the
    /// whitespace/case normalization configured in `config`.
    fn similarity(&self, other: &Self, config: &ApplyConfig) -> f32;

    /// Full equality modulo the whitespace/case normalization configured in
    /// `config`. Cheaper than `similarity(..) == 1.0`.
    fn normalized_eq(&self, other: &Self, config: &ApplyConfig) -> bool {
        self.similarity(other, config) >= 1.0
    }
}

/// Iterator over the chars of `s` with the configured normalization applied.
fn normalized_chars<'a>(s: &'a str, config: &'a FuzzyConfig) -> impl Iterator<Item = char> + 'a {
    s.chars()
        .filter(move |c| !(config.ignore_whitespace && c.is_whitespace()))
        .flat_map(move |c| {
            let mut lower = None;
            let mut this = Some(c);
            if config.ignore_case {
                lower = Some(c.to_lowercase());
                this = None;
            }
            lower.into_iter().flatten().chain(this)
        })
}

/// The configured normalization materialized into an owned `String`, or a
/// borrow when no normalization is enabled.
fn normalize<'a>(s: &'a str, config: &FuzzyConfig) -> Cow<'a, str> {
    if config.ignore_whitespace || config.ignore_case {
        Cow::Owned(normalized_chars(s, config).collect())
    } else {
        Cow::Borrowed(s)
    }
}

impl FuzzyComparable for str {
    fn fuzzy_eq(&self, other: &Self, config: &ApplyConfig) -> bool {
        let fuzzy = &config.fuzzy_config;
        if self.normalized_eq(other, config) {
            return true;
        }
        let threshold = fuzzy.similarity_threshold;
        if threshold >= 1.0 {
            return false;
        }

        let s1 = normalize(self, fuzzy);
        let s2 = normalize(other, fuzzy);
        let max_len = s1.len().max(s2.len());
        if max_len == 0 {
            return true;
        }

        // Cheap upper bound before running O(n*m) Levenshtein: the distance
        // is at least the difference in character counts.
        let chars1 = s1.chars().count();
        let chars2 = s2.chars().count();
        let upper_bound = 1.0 - (chars1.abs_diff(chars2) as f32 / max_len as f32);
        if upper_bound < threshold {
            return false;
        }

        let distance = strsim::levenshtein(&s1, &s2);
        1.0 - (distance as f32 / max_len as f32) >= threshold
    }

    fn similarity(&self, other: &Self, config: &ApplyConfig) -> f32 {
        let fuzzy = &config.fuzzy_config;
        if self.normalized_eq(other, config) {
            return 1.0;
        }

        let s1 = normalize(self, fuzzy);
        let s2 = normalize(other, fuzzy);
        let max_len = s1.len().max(s2.len());
        if max_len == 0 {
            return 1.0;
        }

        let distance = strsim::levenshtein(&s1, &s2);
        1.0 - (distance as f32 / max_len as f32)
    }

    fn normalized_eq(&self, other: &Self, config: &ApplyConfig) -> bool {
        let fuzzy = &config.fuzzy_config;
        if !fuzzy.ignore_whitespace && !fuzzy.ignore_case {
            return self == other;
        }
        normalized_chars(self, fuzzy).eq(normalized_chars(other, fuzzy))
    }
}

impl FuzzyComparable for [u8] {
    fn fuzzy_eq(&self, other: &Self, config: &ApplyConfig) -> bool {
        // Try to convert to UTF-8 strings for better comparison
        if let (Ok(s1), Ok(s2)) = (std::str::from_utf8(self), std::str::from_utf8(other)) {
            s1.fuzzy_eq(s2, config)
        } else {
            // Fall back to exact byte comparison
            self == other
        }
    }

    fn similarity(&self, other: &Self, config: &ApplyConfig) -> f32 {
        // Try to convert to UTF-8 strings for better comparison
        if let (Ok(s1), Ok(s2)) = (std::str::from_utf8(self), std::str::from_utf8(other)) {
            s1.similarity(s2, config)
        } else {
            // Fall back to exact byte comparison
            if self == other { 1.0 } else { 0.0 }
        }
    }

    fn normalized_eq(&self, other: &Self, config: &ApplyConfig) -> bool {
        if let (Ok(s1), Ok(s2)) = (std::str::from_utf8(self), std::str::from_utf8(other)) {
            s1.normalized_eq(s2, config)
        } else {
            self == other
        }
    }
}

#[derive(Debug)]
enum ImageLine<'a, T: ?Sized> {
    Unpatched((&'a T, Option<LineEnd>)),
    Patched((&'a T, Option<LineEnd>)),
}

impl<'a, T: ?Sized + Text> ImageLine<'a, T> {
    fn inner(&self) -> (&T, Option<LineEnd>) {
        match self {
            ImageLine::Unpatched(inner) | ImageLine::Patched(inner) => *inner,
        }
    }

    fn into_inner(self) -> (&'a T, Option<LineEnd>) {
        match self {
            ImageLine::Unpatched(inner) | ImageLine::Patched(inner) => inner,
        }
    }

    fn is_patched(&self) -> bool {
        match self {
            ImageLine::Unpatched(_) => false,
            ImageLine::Patched(_) => true,
        }
    }
}

impl<T: ?Sized> Copy for ImageLine<'_, T> {}

impl<T: ?Sized> Clone for ImageLine<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Apply a `Diff` to a base image with default fuzzy matching
pub fn apply(base_image: &str, diff: &Diff<'_, str>) -> ApplyResult<String, ApplyError> {
    apply_with_config(base_image, diff, &ApplyConfig::default())
}

/// Apply a `Diff` to a base image with custom fuzzy matching configuration
pub fn apply_with_config(
    base_image: &str,
    diff: &Diff<'_, str>,
    config: &ApplyConfig,
) -> ApplyResult<String, ApplyError> {
    let (lines, stats) = apply_all(base_image, diff, config)?;
    Ok((assemble_str(lines, base_image.len()), stats))
}

/// Concatenate resolved output lines into a `String`
fn assemble_str(lines: OutputLines<'_, str>, capacity: usize) -> String {
    let mut content = String::with_capacity(capacity);
    for (line, ending) in lines {
        content.push_str(line);
        if let Some(ending) = ending {
            let e: &str = ending.into();
            content.push_str(e);
        }
    }
    content
}

/// Concatenate resolved output lines into a `Vec<u8>`
fn assemble_bytes(lines: OutputLines<'_, [u8]>, capacity: usize) -> Vec<u8> {
    let mut content = Vec::with_capacity(capacity);
    for (line, ending) in lines {
        content.extend_from_slice(line);
        if let Some(ending) = ending {
            let e: &[u8] = ending.into();
            content.extend_from_slice(e);
        }
    }
    content
}

/// Apply a non-utf8 `Diff` to a base image with default fuzzy matching
pub fn apply_bytes(base_image: &[u8], patch: &Diff<'_, [u8]>) -> ApplyResult<Vec<u8>, ApplyError> {
    apply_bytes_with_config(base_image, patch, &ApplyConfig::default())
}

/// Apply a non-utf8 `Diff` to a base image with custom fuzzy matching configuration
pub fn apply_bytes_with_config(
    base_image: &[u8],
    diff: &Diff<'_, [u8]>,
    config: &ApplyConfig,
) -> ApplyResult<Vec<u8>, ApplyError> {
    let (lines, stats) = apply_all(base_image, diff, config)?;
    Ok((assemble_bytes(lines, base_image.len()), stats))
}

/// A patched file as resolved output lines: the line's content and the line
/// ending it should be written with.
type OutputLines<'a, T> = Vec<(&'a T, Option<LineEnd>)>;

/// The shared application core: patches the image hunk by hunk and resolves
/// the line ending of every output line according to the configured
/// [`LineEndHandling`].
fn apply_all<'a, T>(
    base_image: &'a T,
    diff: &'a Diff<'a, T>,
    config: &ApplyConfig,
) -> Result<(OutputLines<'a, T>, ApplyStats), ApplyError>
where
    T: PartialEq + FuzzyComparable + ?Sized + Text + ToOwned,
    Hunk<'a, T>: fmt::Display,
{
    let (lines, stats, rejected) = apply_all_partial(base_image, diff, config);
    if let Some((index, hunk)) = rejected.into_iter().next() {
        return Err(ApplyError(index, format!("{}", hunk)));
    }
    Ok((lines, stats))
}

/// Rejected hunks with their 1-based index in the diff
type RejectedHunks<'a, T> = Vec<(usize, Hunk<'a, T>)>;

/// Like [`apply_all`], but GNU patch-like: hunks that cannot be placed are
/// skipped instead of failing the whole application, and returned as rejects
/// together with their 1-based index in the diff.
fn apply_all_partial<'a, T>(
    base_image: &'a T,
    diff: &'a Diff<'a, T>,
    config: &ApplyConfig,
) -> (OutputLines<'a, T>, ApplyStats, RejectedHunks<'a, T>)
where
    T: PartialEq + FuzzyComparable + ?Sized + Text + ToOwned,
{
    let mut image: Vec<_> = LineIter::new(base_image)
        .map(ImageLine::Unpatched)
        .collect();

    // A file without any line ending (empty, or a single line with no
    // trailing newline) provides no evidence of a convention, so inserted
    // lines fall back to the patch's own endings instead of a
    // platform-dependent default.
    let file_line_ending = if memchr::memchr(b'\n', base_image.as_bytes()).is_some() {
        Some(LineEnd::most_common(base_image))
    } else {
        None
    };

    let mut stats = ApplyStats::new();
    let mut rejected = Vec::new();

    for (i, hunk) in diff.hunks().iter().enumerate() {
        match apply_hunk_with_config(&mut image, hunk, config, file_line_ending) {
            Ok(hunk_stats) => stats.add_hunk(hunk_stats),
            Err(()) => rejected.push((i + 1, hunk.clone())),
        }
    }

    let preferred_line_ending = match config.line_end_strategy {
        LineEndHandling::KeepOriginal => None,
        LineEndHandling::EnsurePatchLineEnding => {
            let mut lf_score = 0usize;
            let mut crlf_score = 0usize;

            for hunk in diff.hunks().iter() {
                for line in hunk.lines() {
                    match line.line_end() {
                        Some(LineEnd::Lf) => lf_score += 1,
                        Some(LineEnd::CrLf) => crlf_score += 1,
                        _ => (),
                    }
                }
            }

            Some(LineEnd::choose_from_scores(lf_score, crlf_score))
        }
        LineEndHandling::EnsureFileLineEnding => Some(LineEnd::most_common(base_image)),
        LineEndHandling::EnsureLineEnding(line_end) => Some(line_end),
    };

    let lines = image
        .into_iter()
        .map(ImageLine::into_inner)
        .map(|(line, ending)| {
            let ending = match (preferred_line_ending, ending) {
                // A missing final newline is always preserved
                (_, None) => None,
                (Some(preferred), Some(_)) => Some(preferred),
                (None, ending) => ending,
            };
            (line, ending)
        })
        .collect();

    (lines, stats, rejected)
}

/// The result of a partial, GNU patch-like application from [`apply_partial`]
/// or [`apply_bytes_partial`]: every hunk that can be placed is applied, the
/// ones that cannot are returned instead of failing the whole file.
///
/// `rejected` holds the failed hunks in patch order; format them with a
/// [`PatchFormatter`](crate::PatchFormatter) (or via a rebuilt
/// [`Diff`]) to produce a `.rej` file like GNU patch's.
#[derive(Clone, PartialEq, Eq)]
pub struct PartialApply<'a, T: ToOwned + ?Sized, C> {
    /// The content with all applicable hunks applied (equal to the input when
    /// every hunk was rejected)
    pub content: C,
    /// Statistics over the hunks that were applied
    pub stats: ApplyStats,
    /// The hunks that could not be applied, in patch order
    pub rejected: Vec<Hunk<'a, T>>,
}

impl<T, C> fmt::Debug for PartialApply<'_, T, C>
where
    T: ToOwned + ?Sized + fmt::Debug + Text,
    T::Owned: fmt::Debug,
    C: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartialApply")
            .field("content", &self.content)
            .field("stats", &self.stats)
            .field("rejected", &self.rejected)
            .finish()
    }
}

/// Apply a `Diff`, GNU patch-style: hunks that fit are applied, hunks that do
/// not are returned as rejects instead of failing the whole file.
pub fn apply_partial<'a>(
    base_image: &'a str,
    diff: &'a Diff<'a, str>,
    config: &ApplyConfig,
) -> PartialApply<'a, str, String> {
    let (lines, stats, rejected) = apply_all_partial(base_image, diff, config);
    PartialApply {
        content: assemble_str(lines, base_image.len()),
        stats,
        rejected: rejected.into_iter().map(|(_, hunk)| hunk).collect(),
    }
}

/// The bytes twin of [`apply_partial`].
pub fn apply_bytes_partial<'a>(
    base_image: &'a [u8],
    diff: &'a Diff<'a, [u8]>,
    config: &ApplyConfig,
) -> PartialApply<'a, [u8], Vec<u8>> {
    let (lines, stats, rejected) = apply_all_partial(base_image, diff, config);
    PartialApply {
        content: assemble_bytes(lines, base_image.len()),
        stats,
        rejected: rejected.into_iter().map(|(_, hunk)| hunk).collect(),
    }
}

/// Returns `true` if `diff` already appears to be applied to `base_image`,
/// i.e. `base_image` already reflects the *modified* side of the diff
/// ("reversed or previously applied", in GNU patch terms).
///
/// A diff counts as already applied only if every hunk's *post-image* — its
/// context lines together with its inserted lines, in order and contiguous —
/// is found in `base_image`. Context lines are compared modulo the
/// whitespace/case normalization from `config` (no similarity threshold, no
/// fuzz); inserted lines must match byte-for-byte, because they are the only
/// evidence that the hunk was applied — under `ignore_whitespace` a hunk
/// whose insertions differ from its deletions only in whitespace would
/// otherwise be misreported as applied on the *un*patched content.
///
/// Neither a forward apply nor a reverse round-trip is a reliable signal under
/// fuzzy matching. On already-applied content a forward apply may fail (a
/// deleted line no longer matches) *or* succeed while wrongly re-applying the
/// change (e.g. inserting an already-present line a second time). Conversely,
/// reversing a mostly-deleting diff yields an *insertion* diff that can apply
/// to the **un**patched content just as well (duplicating the still-present
/// lines) and round-trip cleanly, misclassifying a not-yet-applied patch as
/// applied. Matching the post-image directly has neither failure mode.
///
/// This is only meaningful for content-modifying diffs; callers should handle
/// pure file creation/deletion/rename at the path level. A hunk that consists
/// solely of deletions with no context is never reported as already applied,
/// because nothing verifiable remains of it in the patched content.
///
/// A hunk with deletions but no insertions needs one more check: its
/// post-image is just its context lines, which the *un*patched content
/// contains too (contiguously, whenever the deletions sit at the hunk's edge).
/// Such a hunk therefore also requires that its *pre-image* — context plus
/// deleted lines — is absent, i.e. the deleted lines are actually gone.
///
/// # Examples
///
/// ```
/// use flickzeug::{is_diff_applied_with_config, ApplyConfig, Diff};
///
/// let patch = "\
/// --- a/version
/// +++ b/version
/// @@ -1 +1 @@
/// -3.1
/// +3.12
/// ";
/// let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
/// let config = ApplyConfig::default();
///
/// // The diff has not been applied to the pre-image.
/// assert!(!is_diff_applied_with_config(b"3.1\n", &diff, &config));
/// // The diff has already been applied to the post-image.
/// assert!(is_diff_applied_with_config(b"3.12\n", &diff, &config));
/// ```
pub fn is_diff_applied_with_config(
    base_image: &[u8],
    diff: &Diff<'_, [u8]>,
    config: &ApplyConfig,
) -> bool {
    is_diff_applied_generic(base_image, diff, config)
}

/// The `str` twin of [`is_diff_applied_with_config`].
pub fn is_diff_applied_str_with_config(
    base_image: &str,
    diff: &Diff<'_, str>,
    config: &ApplyConfig,
) -> bool {
    is_diff_applied_generic(base_image, diff, config)
}

fn is_diff_applied_generic<T>(base_image: &T, diff: &Diff<'_, T>, config: &ApplyConfig) -> bool
where
    T: FuzzyComparable + ?Sized + Text + ToOwned,
{
    let hunks = diff.hunks();
    if hunks.is_empty() {
        return false;
    }

    let image: Vec<(&T, Option<LineEnd>)> = LineIter::new(base_image).collect();

    hunks
        .iter()
        .all(|hunk| is_hunk_applied(&image, hunk, config))
}

/// Returns `true` if `hunk` appears to be already applied to `image`: its
/// post-image occurs contiguously, and — for a hunk that deletes without
/// inserting, whose post-image is only context lines that the unpatched
/// content contains as well — its pre-image (context plus deleted lines)
/// does *not* occur, i.e. the deleted lines are actually gone.
fn is_hunk_applied<T>(
    image: &[(&T, Option<LineEnd>)],
    hunk: &Hunk<'_, T>,
    config: &ApplyConfig,
) -> bool
where
    T: FuzzyComparable + ?Sized + Text + ToOwned,
{
    // Inserted lines are the only evidence that the hunk was actually
    // applied, so they must match byte-for-byte; context lines tolerate the
    // whitespace/case normalization from `config`. Under `ignore_whitespace`
    // a hunk whose insertions differ from its deletions only in whitespace
    // would otherwise normalize to the *un*patched content and be
    // misreported as already applied.
    let post_image_lines: Vec<_> = hunk
        .lines()
        .iter()
        .filter_map(|line| match *line {
            Line::Context(l) => Some((l, false)),
            Line::Insert(l) => Some((l, true)),
            Line::Delete(_) => None,
        })
        .collect();
    if post_image_lines.is_empty() {
        return false;
    }

    let post_start = hunk.new_range().start().saturating_sub(1);
    if find_lines_position(image, &post_image_lines, post_start, config).is_none() {
        return false;
    }

    let has_insertion = hunk
        .lines()
        .iter()
        .any(|line| matches!(line, Line::Insert(_)));
    if has_insertion {
        return true;
    }

    let pre_image_lines: Vec<_> = pre_image(hunk.lines()).map(|line| (line, false)).collect();
    let pre_start = hunk.old_range().start().saturating_sub(1);
    find_lines_position(image, &pre_image_lines, pre_start, config).is_none()
}

/// Search `image` for a position where `lines` occur contiguously. Each line
/// carries an `exact` flag: `true` requires byte equality, `false` allows
/// equality modulo the whitespace/case normalization from `config` (never a
/// similarity threshold). Returns `None` if `lines` is empty.
fn find_lines_position<T>(
    image: &[(&T, Option<LineEnd>)],
    lines: &[((&T, Option<LineEnd>), bool)],
    start_hint: usize,
    config: &ApplyConfig,
) -> Option<usize>
where
    T: FuzzyComparable + ?Sized + Text + ToOwned,
{
    if lines.is_empty() {
        return None;
    }

    let match_at = |pos: usize| -> bool {
        image.get(pos..pos + lines.len()).is_some_and(|window| {
            lines.iter().zip(window).all(|((line, exact), image_line)| {
                // Whether a line ending exists is semantic (the "\ No newline
                // at end of file" marker): a diff that only adds or removes
                // the trailing newline must not count as already applied.
                // Which ending it is (LF vs CRLF) is convention and ignored.
                line.1.is_some() == image_line.1.is_some()
                    && if *exact {
                        line.0 == image_line.0
                    } else {
                        line.0.normalized_eq(image_line.0, config)
                    }
            })
        })
    };

    // Start at the position the hunk header points at and search outward, like
    // the pre-image search in `find_position`.
    let pos = std::cmp::min(start_hint, image.len());
    let backward = (0..pos).rev();
    let forward = pos + 1..image.len();

    iter::once(pos)
        .chain(interleave(backward, forward))
        .find(|&pos| match_at(pos))
}

/// The outcome of attempting to apply a diff with [`apply_bytes_reporting`]
/// or [`apply_reporting`].
///
/// This distinguishes the three cases a caller usually cares about: the diff
/// was applied and changed the content, the diff appears to be already applied
/// (so applying it would be a no-op), or the diff does not apply and is not
/// already applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome<T = Vec<u8>> {
    /// The diff was applied and modified the base image. Contains the patched
    /// image together with the [`ApplyStats`] from the application.
    Applied(T, ApplyStats),
    /// The diff appears to be already applied: `base_image` already reflects the
    /// modified side of the diff. Contains the (unchanged) base image.
    ///
    /// This is detected robustly even under fuzzy matching, where a forward
    /// apply of an already-applied diff succeeds as a no-op.
    AlreadyApplied(T),
    /// The diff could not be applied and is not already applied. Contains the
    /// [`ApplyError`] from the failed forward application.
    Failed(ApplyError),
}

/// Apply a non-utf8 `Diff` to a base image, reporting whether it was applied,
/// was already applied, or failed.
///
/// This is a convenience wrapper around [`is_diff_applied_with_config`] and
/// [`apply_bytes_with_config`] that answers "apply this, but if it is already
/// applied tell me so instead of applying it again". It checks for the
/// already-applied case *first* (by locating each hunk's post-image) and only
/// forward applies when the diff is genuinely not yet applied. This ordering is
/// necessary under fuzzy matching, where a forward apply of an already-applied
/// diff is unreliable — it may fail, or it may succeed while wrongly
/// re-applying the change (e.g. duplicating an inserted line).
///
/// # Examples
///
/// ```
/// use flickzeug::{apply_bytes_reporting, ApplyConfig, ApplyOutcome, Diff};
///
/// let patch = "\
/// --- a/version
/// +++ b/version
/// @@ -1 +1 @@
/// -3.1
/// +3.12
/// ";
/// let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
/// let config = ApplyConfig::default();
///
/// // Fresh content: the diff applies and changes it.
/// assert!(matches!(
///     apply_bytes_reporting(b"3.1\n", &diff, &config),
///     ApplyOutcome::Applied(..)
/// ));
/// // Already-patched content: reported as already applied.
/// assert!(matches!(
///     apply_bytes_reporting(b"3.12\n", &diff, &config),
///     ApplyOutcome::AlreadyApplied(_)
/// ));
/// ```
pub fn apply_bytes_reporting(
    base_image: &[u8],
    diff: &Diff<'_, [u8]>,
    config: &ApplyConfig,
) -> ApplyOutcome {
    // Check for the already-applied case first: a forward apply cannot be
    // trusted to detect it (it may fail, or succeed while re-applying the
    // change), so we must not forward apply until we know the diff is not
    // already applied.
    if is_diff_applied_with_config(base_image, diff, config) {
        return ApplyOutcome::AlreadyApplied(base_image.to_vec());
    }

    match apply_bytes_with_config(base_image, diff, config) {
        Ok((patched, stats)) => ApplyOutcome::Applied(patched, stats),
        Err(err) => ApplyOutcome::Failed(err),
    }
}

/// The `str` twin of [`apply_bytes_reporting`].
pub fn apply_reporting(
    base_image: &str,
    diff: &Diff<'_, str>,
    config: &ApplyConfig,
) -> ApplyOutcome<String> {
    if is_diff_applied_str_with_config(base_image, diff, config) {
        return ApplyOutcome::AlreadyApplied(base_image.to_owned());
    }

    match apply_with_config(base_image, diff, config) {
        Ok((patched, stats)) => ApplyOutcome::Applied(patched, stats),
        Err(err) => ApplyOutcome::Failed(err),
    }
}

fn apply_hunk_with_config<'a, T>(
    image: &mut Vec<ImageLine<'a, T>>,
    hunk: &Hunk<'a, T>,
    config: &ApplyConfig,
    file_line_ending: Option<LineEnd>,
) -> Result<HunkStats, ()>
where
    T: PartialEq + FuzzyComparable + ?Sized + Text + ToOwned,
{
    // Find position with fuzzy matching
    let (pos, fuzz_level) = find_position_fuzzy(image, hunk, config).ok_or(())?;

    // Count changes in this hunk
    let mut added = 0;
    let mut deleted = 0;
    let mut context = 0;

    for line in hunk.lines() {
        match line {
            Line::Insert(_) => added += 1,
            Line::Delete(_) => deleted += 1,
            Line::Context(_) => context += 1,
        }
    }

    let keep_original = matches!(config.line_end_strategy, LineEndHandling::KeepOriginal);

    // update image
    if fuzz_level == 0 && !keep_original {
        // Exact match - replace all lines as before
        image.splice(
            pos..pos + pre_image_line_count(hunk.lines()),
            post_image(hunk.lines()).map(ImageLine::Patched),
        );
    } else {
        // Only remap the endings of inserted lines when the patch's endings
        // demonstrably disagree with the file's at the match site (an LF
        // patch applied to a CRLF file, or vice versa). When they agree, the
        // patch's inserted endings are authoritative — this keeps
        // apply(a, create_patch(a, b)) == b exact even for files that mix
        // line endings.
        let inherit_insert_endings = keep_original && !patch_endings_match_file(image, hunk, pos);

        // Preserve original context lines (and, with KeepOriginal, their
        // line endings), only apply insertions/deletions
        apply_hunk_preserving_context(image, hunk, pos, inherit_insert_endings, file_line_ending);
    }

    Ok(HunkStats {
        added,
        deleted,
        context,
    })
}

/// Returns `true` when every context/deleted line of `hunk` that has a line
/// ending in the patch agrees with the ending of the image line it matched.
/// When they all agree the patch was written with the file's line-ending
/// convention and its inserted endings can be trusted verbatim.
///
/// A hunk with no comparable endings at all (e.g. an insert-only `-U0` hunk)
/// provides no evidence either way and returns `false`, so inserted lines
/// inherit from their neighbors as the `KeepOriginal` documentation promises.
fn patch_endings_match_file<T>(image: &[ImageLine<T>], hunk: &Hunk<'_, T>, pos: usize) -> bool
where
    T: ?Sized + Text + ToOwned,
{
    let mut saw_comparison = false;
    let mut offset = 0;
    for line in hunk.lines() {
        match *line {
            Line::Context((_, end)) | Line::Delete((_, end)) => {
                if let Some(image_line) = image.get(pos + offset) {
                    let image_end = image_line.inner().1;
                    if end.is_some() && image_end.is_some() {
                        saw_comparison = true;
                        if end != image_end {
                            return false;
                        }
                    }
                }
                offset += 1;
            }
            Line::Insert(_) => {}
        }
    }
    saw_comparison
}

/// Apply hunk while preserving original context lines.
///
/// When `inherit_insert_endings` is set (the [`LineEndHandling::KeepOriginal`]
/// strategy), inserted lines take the ending of the closest retained line —
/// looking backwards first, then forwards — falling back to the file's
/// dominant ending, so a `\n` patch inserts `\r\n` lines into a `\r\n` file.
/// An inserted line the patch marks as having no ending keeps that (it is the
/// "\ No newline at end of file" case).
fn apply_hunk_preserving_context<'a, T>(
    image: &mut Vec<ImageLine<'a, T>>,
    hunk: &Hunk<'a, T>,
    pos: usize,
    inherit_insert_endings: bool,
    file_line_ending: Option<LineEnd>,
) where
    T: ?Sized + Text + ToOwned,
{
    let mut image_offset = 0;

    for line in hunk.lines() {
        match *line {
            Line::Context(_) => {
                // Keep the original context line, just mark it as patched
                if let Some(img_line) = image.get_mut(pos + image_offset) {
                    *img_line = ImageLine::Patched(img_line.into_inner());
                }
                image_offset += 1;
            }
            Line::Delete(_) => {
                // Remove the line
                image.remove(pos + image_offset);
            }
            Line::Insert((text, ending)) => {
                let ending = if inherit_insert_endings && ending.is_some() {
                    let previous = (pos + image_offset)
                        .checked_sub(1)
                        .and_then(|i| image.get(i))
                        .and_then(|l| l.inner().1);
                    let next = image.get(pos + image_offset).and_then(|l| l.inner().1);
                    previous.or(next).or(file_line_ending).or(ending)
                } else {
                    ending
                };
                image.insert(pos + image_offset, ImageLine::Patched((text, ending)));
                image_offset += 1;
            }
        }
    }
}

/// Search in `image` for a place to apply hunk with fuzzy matching support
fn find_position_fuzzy<T>(
    image: &[ImageLine<T>],
    hunk: &Hunk<'_, T>,
    config: &ApplyConfig,
) -> Option<(usize, usize)>
where
    T: PartialEq + FuzzyComparable + ?Sized + Text + ToOwned,
{
    // Try exact match first (fuzz level 0)
    if let Some(pos) = find_position(image, hunk) {
        return Some((pos, 0));
    }

    // Precompute everything that is per-hunk rather than per-position once
    let matcher = HunkMatcher::new(hunk);

    // Try fuzzy matching with increasing fuzz levels
    for fuzz_level in 1..=config.fuzzy_config.max_fuzz {
        if let Some(pos) = matcher.find_position(image, hunk, fuzz_level, config) {
            return Some((pos, fuzz_level));
        }
    }

    None
}

/// Per-hunk matching data, computed once per hunk instead of once per
/// candidate position (the position search is O(file length)).
struct HunkMatcher<'a, T: ?Sized> {
    /// The hunk's pre-image: its context and deleted lines, in order
    pre_image: Vec<(&'a T, Option<LineEnd>)>,
    /// Parallel to `pre_image`: whether the line is a deletion
    is_delete: Vec<bool>,
    /// Indices into `pre_image` that are context lines
    context_indices: Vec<usize>,
}

impl<'a, T> HunkMatcher<'a, T>
where
    T: PartialEq + FuzzyComparable + ?Sized + Text + ToOwned,
{
    fn new(hunk: &Hunk<'a, T>) -> Self {
        let mut pre_image = Vec::new();
        let mut is_delete = Vec::new();
        let mut context_indices = Vec::new();

        for line in hunk.lines() {
            match *line {
                Line::Context(l) => {
                    context_indices.push(pre_image.len());
                    pre_image.push(l);
                    is_delete.push(false);
                }
                Line::Delete(l) => {
                    pre_image.push(l);
                    is_delete.push(true);
                }
                Line::Insert(_) => {}
            }
        }

        Self {
            pre_image,
            is_delete,
            context_indices,
        }
    }

    fn find_position(
        &self,
        image: &[ImageLine<T>],
        hunk: &Hunk<'_, T>,
        fuzz_level: usize,
        config: &ApplyConfig,
    ) -> Option<usize> {
        let combinations = self.fuzz_combinations(fuzz_level);

        let pos = std::cmp::min(hunk.new_range().start().saturating_sub(1), image.len());
        let backward = (0..pos).rev();
        let forward = pos + 1..image.len();

        iter::once(pos)
            .chain(interleave(backward, forward))
            .find(|&pos| self.matches_at(image, pos, &combinations, fuzz_level, config))
    }

    /// Generate the combinations of context line indices to ignore that are
    /// *new* at `fuzz_level`, using GNU patch-style edge fuzz: fuzz N may
    /// ignore up to N context lines from the start and up to N from the end.
    /// Combinations already tried at lower levels are not repeated; the empty
    /// combination is tried at level 1 because level 0 uses strict equality
    /// while the fuzzy path compares with the configured normalization and
    /// similarity threshold.
    fn fuzz_combinations(&self, fuzz_level: usize) -> Vec<Vec<usize>> {
        let indices = &self.context_indices;
        if fuzz_level == 0 || indices.is_empty() {
            return vec![vec![]];
        }

        let len = indices.len();
        let mut combinations = Vec::new();

        if fuzz_level == 1 {
            combinations.push(Vec::new());
        }

        for start_ignore in 0..=fuzz_level.min(len) {
            for end_ignore in 0..=fuzz_level.min(len.saturating_sub(start_ignore)) {
                // Only combinations where at least one side reaches the
                // current level are new; the rest were tried at lower levels.
                if start_ignore.max(end_ignore) != fuzz_level {
                    continue;
                }

                let mut ignored = Vec::new();
                ignored.extend(indices.iter().take(start_ignore).copied());
                ignored.extend(
                    indices
                        .iter()
                        .skip(start_ignore)
                        .rev()
                        .take(end_ignore)
                        .copied(),
                );

                combinations.push(ignored);
            }
        }

        combinations
    }

    fn matches_at(
        &self,
        image: &[ImageLine<T>],
        pos: usize,
        combinations: &[Vec<usize>],
        fuzz_level: usize,
        config: &ApplyConfig,
    ) -> bool {
        let Some(window) = image.get(pos..pos + self.pre_image.len()) else {
            return false;
        };

        // If any of these lines have already been patched then we can't match
        // at this position
        if window.iter().any(ImageLine::is_patched) {
            return false;
        }

        // Not enough context lines to perform edge fuzz at this level: fall
        // back to comparing the whole window without ignoring any lines.
        if self.context_indices.len() < fuzz_level {
            return self.window_matches(window, &[], config);
        }

        combinations
            .iter()
            .any(|ignored| self.window_matches(window, ignored, config))
    }

    /// Compare the hunk's pre-image against an image window, skipping the
    /// `ignored` context indices. Context lines match with the configured
    /// similarity threshold; deleted lines always require full (normalized)
    /// equality — a patch must never delete a line other than the one it
    /// names.
    fn window_matches(
        &self,
        window: &[ImageLine<T>],
        ignored: &[usize],
        config: &ApplyConfig,
    ) -> bool {
        self.pre_image
            .iter()
            .zip(window.iter().map(ImageLine::inner))
            .enumerate()
            .all(|(i, (pre_line, image_line))| {
                if ignored.contains(&i) {
                    return true;
                }
                if self.is_delete[i] {
                    pre_line.0.normalized_eq(image_line.0, config)
                } else {
                    pre_line.0.fuzzy_eq(image_line.0, config)
                }
            })
    }
}

// Search in `image` for a place to apply hunk.
// This follows the general algorithm (minus fuzzy-matching context lines) described in GNU patch's
// man page.
//
// It might be worth looking into other possible positions to apply the hunk to as described here:
// https://neil.fraser.name/writing/patch/
fn find_position<T: PartialEq + ?Sized + Text + ToOwned>(
    image: &[ImageLine<T>],
    hunk: &Hunk<'_, T>,
) -> Option<usize> {
    // In order to avoid searching through positions which are out of bounds of the image,
    // clamp the starting position based on the length of the image
    let pos = std::cmp::min(hunk.new_range().start().saturating_sub(1), image.len());

    // Create an iterator that starts with 'pos' and then interleaves
    // moving pos backward/foward by one.
    let backward = (0..pos).rev();
    let forward = pos + 1..image.len();

    iter::once(pos)
        .chain(interleave(backward, forward))
        .find(|&pos| match_fragment(image, hunk.lines(), pos))
}

fn pre_image_line_count<T: ?Sized>(lines: &[Line<'_, T>]) -> usize {
    pre_image(lines).count()
}

fn post_image<'a, 'b, T: ?Sized>(
    lines: &'b [Line<'a, T>],
) -> impl Iterator<Item = (&'a T, Option<LineEnd>)> + 'b {
    lines.iter().filter_map(move |line| match *line {
        Line::Context(l) | Line::Insert(l) => Some(l),
        Line::Delete(_) => None,
    })
}

fn pre_image<'a, 'b: 'a, T: ?Sized>(
    lines: &'b [Line<'a, T>],
) -> impl Iterator<Item = (&'a T, Option<LineEnd>)> + 'b {
    lines.iter().filter_map(|line| match *line {
        Line::Context(l) | Line::Delete(l) => Some(l),
        Line::Insert(_) => None,
    })
}

fn match_fragment<T: PartialEq + ?Sized + Text>(
    image: &[ImageLine<T>],
    lines: &[Line<'_, T>],
    pos: usize,
) -> bool {
    let len = pre_image_line_count(lines);

    let image = if let Some(image) = image.get(pos..pos + len) {
        image
    } else {
        return false;
    };

    // If any of these lines have already been patched then we can't match at this position
    if image.iter().any(ImageLine::is_patched) {
        return false;
    }

    // Compare content only: a difference that is purely in the line endings
    // (LF patch against a CRLF file and vice versa) must not prevent a match.
    pre_image(lines)
        .map(|(line, _end)| line)
        .eq(image.iter().map(|line| line.inner().0))
}

#[derive(Debug)]
struct Interleave<I, J> {
    a: iter::Fuse<I>,
    b: iter::Fuse<J>,
    flag: bool,
}

fn interleave<I, J>(
    i: I,
    j: J,
) -> Interleave<<I as IntoIterator>::IntoIter, <J as IntoIterator>::IntoIter>
where
    I: IntoIterator,
    J: IntoIterator<Item = I::Item>,
{
    Interleave {
        a: i.into_iter().fuse(),
        b: j.into_iter().fuse(),
        flag: false,
    }
}

impl<I, J> Iterator for Interleave<I, J>
where
    I: Iterator,
    J: Iterator<Item = I::Item>,
{
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        self.flag = !self.flag;
        if self.flag {
            match self.a.next() {
                None => self.b.next(),
                item => item,
            }
        } else {
            match self.b.next() {
                None => self.a.next(),
                item => item,
            }
        }
    }
}

#[cfg(test)]
mod test {
    use std::path::PathBuf;

    use crate::{
        ApplyConfig, ApplyOutcome, Diff, FuzzyConfig, LineEndHandling, apply,
        apply_bytes_reporting, apply_reporting, is_diff_applied_with_config,
    };

    fn load_files(name: &str) -> (String, String) {
        let base_folder = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("test-data")
            .join(name);

        let base_image = std::fs::read_to_string(base_folder.join("target.txt")).unwrap();
        let patch = std::fs::read_to_string(base_folder.join("patch.patch")).unwrap();
        (base_image, patch)
    }

    #[test]
    fn apply_patch() {
        let (base_image, patch) = load_files("fuzzy");
        let patch = crate::Diff::from_bytes(patch.as_bytes()).unwrap();

        println!("Applied: {:#?}", patch);
        let (content, _stats) = crate::apply_bytes(base_image.as_bytes(), &patch).unwrap();
        // take the first 50 lines for snapshot testing
        let result = String::from_utf8(content)
            .unwrap()
            .lines()
            .take(50)
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(result);
        println!("Result:\n{}", result);
    }

    fn assert_patch(old: &str, new: &str, patch: &str) {
        let diff = Diff::from_str(patch).unwrap();
        let (content, _stats) = apply(old, &diff).unwrap();
        assert_eq!(new, content);
    }

    #[test]
    fn test_apply_result_statistics() {
        let old = "line 1\nline 2\nline 3\n";
        let new = "line 1\nline 2 modified\nline 4\n";
        let patch = "\
--- original
+++ modified
@@ -1,3 +1,3 @@
 line 1
-line 2
-line 3
+line 2 modified
+line 4
";
        let diff = Diff::from_str(patch).unwrap();
        let (content, stats) = apply(old, &diff).unwrap();

        assert_eq!(content, new);
        assert_eq!(stats.lines_added, 2);
        assert_eq!(stats.lines_deleted, 2);
        assert_eq!(stats.lines_context, 1);
        assert_eq!(stats.hunks_applied, 1);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_apply_result_no_changes() {
        let old = "line 1\nline 2\n";
        let new = "line 1\nline 2\n";
        let patch = "\
--- original
+++ modified
@@ -1,2 +1,2 @@
 line 1
 line 2
";
        let diff = Diff::from_str(patch).unwrap();
        let (content, stats) = apply(old, &diff).unwrap();

        assert_eq!(content, new);
        assert_eq!(stats.lines_added, 0);
        assert_eq!(stats.lines_deleted, 0);
        assert_eq!(stats.lines_context, 2);
        assert_eq!(stats.hunks_applied, 1);
        assert!(!stats.has_changes());
    }

    #[test]
    fn test_apply_result_multiple_hunks() {
        let old = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let new = "line 1\nline 2 modified\nline 3\nline 4 modified\nline 5\n";
        let patch = "\
--- original
+++ modified
@@ -1,2 +1,2 @@
 line 1
-line 2
+line 2 modified
@@ -4,2 +4,2 @@
-line 4
+line 4 modified
 line 5
";
        let diff = Diff::from_str(patch).unwrap();
        let (content, stats) = apply(old, &diff).unwrap();

        assert_eq!(content, new);
        assert_eq!(stats.lines_added, 2);
        assert_eq!(stats.lines_deleted, 2);
        assert_eq!(stats.lines_context, 2);
        assert_eq!(stats.hunks_applied, 2);
        assert!(stats.has_changes());
    }

    #[test]
    fn test_detect_already_applied_patch() {
        let old = "line 1\nline 2\nline 3\n";
        let patch = "\
--- original
+++ modified
@@ -1,3 +1,3 @@
 line 1
-line 2
+line 2 modified
 line 3
";
        let diff = Diff::from_str(patch).unwrap();

        // First application should succeed with changes
        let (content, stats) = apply(old, &diff).unwrap();
        assert_eq!(content, "line 1\nline 2 modified\nline 3\n");
        assert!(stats.has_changes());
        assert_eq!(stats.lines_added, 1);
        assert_eq!(stats.lines_deleted, 1);

        // Second application should fail because the patch expects "line 2" but finds "line 2 modified"
        let result = apply(&content, &diff);
        assert!(result.is_err(), "Applying the same patch twice should fail");
    }

    #[test]
    fn line_end_strategies() {
        let old = "old line\r\n";
        let new = "new line\r\n";
        let patch = "\
--- original
+++ modified
@@ -1 +1 @@
-old line
+new line
";
        assert_patch(old, new, patch);

        let old = "old line\n";
        let new = "new line\n";
        let expected = "\
--- original
+++ modified
@@ -1 +1 @@
-old line
+new line
"
        .replace("\n", "\r\n");
        assert_patch(old, new, expected.as_str());
    }

    #[test]
    fn keep_original_preserves_untouched_line_endings() {
        // A file with mixed line endings: only the patched line may change.
        // The default strategy used to normalize every line to the file's
        // most common ending.
        let base = "keep\r\nold\r\nplain\nmore\r\n";
        let patch = "\
--- a
+++ b
@@ -2 +2 @@
-old
+new
";
        let diff = Diff::from_str(patch).unwrap();
        let (out, _) = apply(base, &diff).unwrap();
        // 'plain' keeps its LF; the inserted line inherits CRLF from its
        // neighborhood even though the patch uses LF.
        assert_eq!(out, "keep\r\nnew\r\nplain\nmore\r\n");
    }

    #[test]
    fn keep_original_inserted_lines_inherit_neighbor_ending() {
        let base = "one\r\ntwo\r\nthree\r\n";
        let patch = "\
--- a
+++ b
@@ -1,3 +1,4 @@
 one
 two
+two and a half
 three
";
        let diff = Diff::from_str(patch).unwrap();
        let (out, _) = apply(base, &diff).unwrap();
        assert_eq!(out, "one\r\ntwo\r\ntwo and a half\r\nthree\r\n");
    }

    #[test]
    fn keep_original_zero_context_insert_inherits_ending() {
        // Adversarial-review finding: an insert-only hunk (-U0 style) has no
        // context/delete endings to compare, so the patch's endings were
        // trusted vacuously and an LF line was inserted into a CRLF file.
        let base = "one\r\ntwo\r\nthree\r\n";
        let patch = "\
--- a
+++ b
@@ -1,0 +2 @@
+inserted
";
        let diff = Diff::from_str(patch).unwrap();
        let (out, _) = apply(base, &diff).unwrap();
        assert_eq!(out, "one\r\ninserted\r\ntwo\r\nthree\r\n");
    }

    #[test]
    fn keep_original_endingless_file_keeps_patch_ending() {
        // A base file with no line endings at all gives no convention to
        // inherit; the inserted line must keep the patch's ending on every
        // platform (LineEnd::most_common would tie-break on cfg!(windows)).
        let base = "old line";
        let patch = "\
--- a
+++ b
@@ -1 +1 @@
-old line
\\ No newline at end of file
+new line
";
        let diff = Diff::from_str(patch).unwrap();
        let (out, _) = apply(base, &diff).unwrap();
        assert_eq!(out, "new line\n");
    }

    #[test]
    fn keep_original_preserves_missing_final_newline() {
        // The inserted final line is marked "no newline" in the patch and
        // must stay that way even though its neighbors have endings.
        let base = "a\nb\n";
        let patch = "\
--- a
+++ b
@@ -1,2 +1,3 @@
 a
 b
+no newline here
\\ No newline at end of file
";
        let diff = Diff::from_str(patch).unwrap();
        let (out, _) = apply(base, &diff).unwrap();
        assert_eq!(out, "a\nb\nno newline here");
    }

    #[test]
    fn ensure_file_line_ending_still_normalizes() {
        // The old default remains available as an explicit opt-in.
        let base = "keep\r\nold\r\nplain\nmore\r\n";
        let patch = "\
--- a
+++ b
@@ -2 +2 @@
-old
+new
";
        let diff = Diff::from_str(patch).unwrap();
        let config = ApplyConfig {
            line_end_strategy: LineEndHandling::EnsureFileLineEnding,
            ..ApplyConfig::default()
        };
        let (out, _) = crate::apply_with_config(base, &diff, &config).unwrap();
        assert_eq!(out, "keep\r\nnew\r\nplain\r\nmore\r\n");
    }

    #[test]
    fn delete_lines_require_exact_match() {
        // The deleted line in the patch is only similar (not equal) to the
        // line in the file. Context matches exactly. GNU patch rejects this;
        // similarity-based matching used to delete the wrong line.
        let base = "context 1\nthe quick brown fox jumps\ncontext 2\n";
        let patch = "\
--- a
+++ b
@@ -1,3 +1,2 @@
 context 1
-the quick brown fox jumped
 context 2
";
        let diff = Diff::from_str(patch).unwrap();
        assert!(apply(base, &diff).is_err());
    }

    #[test]
    fn similarity_threshold_is_configurable() {
        // One context line differs slightly; with fuzz the hunk applies when
        // the threshold tolerates the difference and fails when set to 1.0.
        let base = "int main(void) {\nreturn 0;\n}\n";
        let patch = "\
--- a
+++ b
@@ -1,3 +1,4 @@
 int main(void)  {
 return 0;
+// done
 }
";
        let diff = Diff::from_str(patch).unwrap();

        let lenient = ApplyConfig::default();
        assert!(crate::apply_with_config(base, &diff, &lenient).is_ok());

        let strict = ApplyConfig {
            fuzzy_config: FuzzyConfig {
                similarity_threshold: 1.0,
                ..FuzzyConfig::default()
            },
            ..ApplyConfig::default()
        };
        // With threshold 1.0 the mismatching context line can still be
        // ignored by edge fuzz, so this applies; disallow fuzz too and it
        // must fail.
        let exact = ApplyConfig {
            fuzzy_config: FuzzyConfig {
                similarity_threshold: 1.0,
                max_fuzz: 0,
                ..FuzzyConfig::default()
            },
            ..ApplyConfig::default()
        };
        assert!(crate::apply_with_config(base, &diff, &strict).is_ok());
        assert!(crate::apply_with_config(base, &diff, &exact).is_err());
    }

    #[test]
    fn test_error_message_format() {
        // Test that error messages show the hunk in a readable format
        let base = "completely different content\n";
        let patch = "\
--- original
+++ modified
@@ -1,3 +1,4 @@
 line 1
-line 2
+line 2 modified
+new line
 line 3
";
        let diff = Diff::from_str(patch).unwrap();
        let result = apply(base, &diff);
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert_eq!(err.hunk_index(), 1);
        assert!(err.hunk_content().contains("line 2 modified"));
        let err_msg = err.to_string();

        // Snapshot test the error message format
        insta::assert_snapshot!(err_msg);
    }

    #[test]
    fn test_tectonic_patch_with_fuzz() {
        // Test case from real-world patch that succeeds with GNU patch-style edge fuzz.
        // The patch expects different versions (^0.5 vs ^0.7/^0.6) but with fuzz 2,
        // we can ignore 2 context lines from start and 2 from end, leaving only
        // the blank line and [features] which do match.
        let (base_image, patch) = load_files("tectonic");
        let diff = crate::Diff::from_str(&patch).unwrap();
        let (result, stats) = crate::apply(&base_image, &diff)
            .expect("Patch should succeed with GNU patch-style fuzz");

        // Verify the patch was applied
        assert!(stats.has_changes());
        assert_eq!(stats.hunks_applied, 1);

        // The patch should have inserted the [patch.crates-io] section
        assert!(
            result.contains("[patch.crates-io]"),
            "Patched file should contain [patch.crates-io]"
        );
        assert!(
            result.contains("libz-sys"),
            "Patched file should contain libz-sys"
        );

        // Snapshot the relevant portion around the inserted lines
        let relevant_lines: String = result
            .lines()
            .skip(97) // Skip to around where the patch was applied
            .take(10)
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(relevant_lines);
    }

    /// rattler-build's configuration: fuzzy matching enabled, which is exactly
    /// the case where a naive forward-apply check misclassifies an
    /// already-applied diff (the forward apply succeeds as a no-op).
    fn fuzzy_config() -> ApplyConfig {
        ApplyConfig {
            fuzzy_config: FuzzyConfig {
                max_fuzz: 2,
                ignore_whitespace: true,
                ignore_case: false,
                ..FuzzyConfig::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn test_is_diff_applied_line_replacement() {
        let patch = "\
--- a/version
+++ b/version
@@ -1,3 +1,3 @@
 line 1
-3.1
+3.12
 line 3
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"line 1\n3.1\nline 3\n";
        let post = b"line 1\n3.12\nline 3\n";

        // Not applied to the pre-image.
        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        // Already applied to the post-image.
        assert!(is_diff_applied_with_config(post, &diff, &config));
    }

    #[test]
    fn test_is_diff_applied_pure_insertion() {
        // A hunk with only context + inserted lines (no removed lines). This is
        // the case most likely to fool a forward-based check, because inserting
        // already-present lines under fuzzy matching can look like a no-op.
        let patch = "\
--- a/list
+++ b/list
@@ -1,3 +1,4 @@
 first
 second
+inserted
 third
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"first\nsecond\nthird\n";
        let post = b"first\nsecond\ninserted\nthird\n";

        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        assert!(is_diff_applied_with_config(post, &diff, &config));
    }

    #[test]
    fn test_is_diff_applied_whitespace_only_change() {
        // A patch whose inserted lines differ from the deleted ones only in
        // whitespace (here: srsly's JSON tests gaining a space after each
        // key). Under `ignore_whitespace` the whole change vanishes when
        // normalized, so a normalized post-image search finds the *un*patched
        // content and misreports the diff as already applied — silently
        // skipping a patch that is real and required.
        let patch = "\
--- a/srsly/tests/test_json_api.py
+++ b/srsly/tests/test_json_api.py
@@ -1,4 +1,4 @@
 expected = [
-    '{\"hello\":\"world\"}',
-    '{\"test\":123}',
+    '{\"hello\": \"world\"}',
+    '{\"test\": 123}',
 ]
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre: &[u8] = b"expected = [\n    '{\"hello\":\"world\"}',\n    '{\"test\":123}',\n]\n";
        let post: &[u8] =
            b"expected = [\n    '{\"hello\": \"world\"}',\n    '{\"test\": 123}',\n]\n";

        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        assert!(is_diff_applied_with_config(post, &diff, &config));

        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, _) => assert_eq!(content, post),
            other => panic!("expected Applied, got {other:?}"),
        }
        match apply_bytes_reporting(post, &diff, &config) {
            ApplyOutcome::AlreadyApplied(content) => assert_eq!(content, post),
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
    }

    #[test]
    fn test_is_diff_applied_unrelated_content() {
        let patch = "\
--- a/version
+++ b/version
@@ -1 +1 @@
-3.1
+3.12
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        // Neither the pre- nor the post-image: not applied.
        assert!(!is_diff_applied_with_config(
            b"something else entirely\n",
            &diff,
            &config
        ));
    }

    #[test]
    fn test_forward_reapply_is_unreliable_but_classifier_is_correct() {
        // Under fuzzy matching a forward re-apply of an already-applied diff is
        // an unreliable signal: depending on the hunk shape it either fails or
        // succeeds while wrongly re-applying the change. The classifier must be
        // correct regardless.
        let config = fuzzy_config();

        // Line replacement: forward re-apply FAILS (the `-3.1` delete line no
        // longer matches the already-patched `3.12`).
        let replace = "\
--- a/version
+++ b/version
@@ -1,3 +1,3 @@
 line 1
-3.1
+3.12
 line 3
";
        let replace_diff = Diff::from_bytes(replace.as_bytes()).unwrap();
        let replace_post = b"line 1\n3.12\nline 3\n";
        assert!(crate::apply_bytes_with_config(replace_post, &replace_diff, &config).is_err());
        assert!(is_diff_applied_with_config(
            replace_post,
            &replace_diff,
            &config
        ));

        // Pure insertion: forward re-apply SUCCEEDS but wrongly re-inserts the
        // already-present line, changing the content.
        let insert = "\
--- a/list
+++ b/list
@@ -1,3 +1,4 @@
 first
 second
+inserted
 third
";
        let insert_diff = Diff::from_bytes(insert.as_bytes()).unwrap();
        let insert_post = b"first\nsecond\ninserted\nthird\n";
        let (reapplied, _) =
            crate::apply_bytes_with_config(insert_post, &insert_diff, &config).unwrap();
        assert_ne!(&reapplied[..], &insert_post[..]);
        assert!(is_diff_applied_with_config(
            insert_post,
            &insert_diff,
            &config
        ));
    }

    #[test]
    fn test_is_diff_applied_pure_deletion() {
        // A hunk that only deletes lines. This is the case most likely to fool
        // a reverse-round-trip check: the reversed diff is a pure insertion,
        // which also applies to the *un*patched content (duplicating the
        // still-present lines) and round-trips cleanly.
        // https://github.com/prefix-dev/rattler-build/issues/2693
        let patch = "\
--- a/CMakeLists.txt
+++ b/CMakeLists.txt
@@ -1,7 +1,4 @@
 else()
-    if (CMAKE_BUILD_TYPE MATCHES Debug)
-        add_compile_options(/MTd)
-    endif()
     add_compile_options(/utf-8)
     add_compile_definitions(-D_CRT_SECURE_NO_WARNINGS=1)
 endif()
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"\
else()
    if (CMAKE_BUILD_TYPE MATCHES Debug)
        add_compile_options(/MTd)
    endif()
    add_compile_options(/utf-8)
    add_compile_definitions(-D_CRT_SECURE_NO_WARNINGS=1)
endif()
";
        let post = b"\
else()
    add_compile_options(/utf-8)
    add_compile_definitions(-D_CRT_SECURE_NO_WARNINGS=1)
endif()
";

        // The unpatched content still contains the lines to delete: NOT applied.
        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        // The patched content no longer contains them: already applied.
        assert!(is_diff_applied_with_config(post, &diff, &config));

        // And the reporting wrapper must actually apply it, not skip it.
        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, _) => assert_eq!(content, post),
            other => panic!("expected Applied, got {other:?}"),
        }
        match apply_bytes_reporting(post, &diff, &config) {
            ApplyOutcome::AlreadyApplied(content) => assert_eq!(content, post),
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
    }

    #[test]
    fn test_pure_deletion_without_trailing_context_is_not_already_applied() {
        // Deletion-only hunk with leading context and NO trailing context: the
        // post-image is just the context line, which the unpatched file also
        // contains, so a post-image match alone must not count as applied.
        // Distilled from php-feedstock's 0001-win-iconv-compat.patch, which
        // empties php_iconv.def (`EXPORTS` context followed by deletions to
        // EOF) and was silently skipped.
        let patch = "\
--- a/file
+++ b/file
@@ -1,6 +1,1 @@
 l1
-l2
-l3
-l4
-l5
-l6
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"l1\nl2\nl3\nl4\nl5\nl6\n";
        let post = b"l1\n";

        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        assert!(is_diff_applied_with_config(post, &diff, &config));

        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, _) => assert_eq!(content, post),
            other => panic!("expected Applied, got {other:?}"),
        }
        match apply_bytes_reporting(post, &diff, &config) {
            ApplyOutcome::AlreadyApplied(content) => assert_eq!(content, post),
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
    }

    #[test]
    fn test_pure_deletion_mid_file_without_trailing_context_is_not_already_applied() {
        // Same shape as above but the deletion is in the middle of the file,
        // not at EOF.
        let patch = "\
--- a/file
+++ b/file
@@ -2,2 +2,1 @@
 l2
-l3
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"l1\nl2\nl3\nl4\nl5\n";
        let post = b"l1\nl2\nl4\nl5\n";

        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        assert!(is_diff_applied_with_config(post, &diff, &config));

        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, _) => assert_eq!(content, post),
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn test_pure_deletion_without_leading_context_is_not_already_applied() {
        // Mirror case: deletion at the START of the hunk with only trailing
        // context. The post-image is just the trailing context line, which the
        // unpatched file contains as well.
        let patch = "\
--- a/file
+++ b/file
@@ -2,2 +2,1 @@
-l2
 l3
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"l1\nl2\nl3\n";
        let post = b"l1\nl3\n";

        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        assert!(is_diff_applied_with_config(post, &diff, &config));

        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, _) => assert_eq!(content, post),
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn test_deletion_patch_with_stale_context_is_not_already_applied() {
        // Deletion-only hunk whose trailing context does not match the file
        // (the patch was written against a newer upstream). GNU patch rejects
        // this; it must be reported as Failed, never as AlreadyApplied.
        // Distilled from the libddwaf win-rt.patch case in
        // https://github.com/prefix-dev/rattler-build/issues/2693
        let patch = "\
--- a/CMakeLists.txt
+++ b/CMakeLists.txt
@@ -1,10 +1,4 @@
 else()
-    if (CMAKE_BUILD_TYPE MATCHES Debug)
-        add_compile_options(/MTd)
-    else()
-        add_compile_options(/MT)
-    endif()
-
     add_compile_options(/utf-8)
     add_compile_definitions(-D_CRT_SECURE_NO_WARNINGS=1)
 endif()
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        // The actual file has no `add_compile_options(/utf-8)` line, so the
        // hunk's context can never fully match.
        let base = b"\
else()
    if (CMAKE_BUILD_TYPE MATCHES Debug)
        add_compile_options(/MTd)
    else()
        add_compile_options(/MT)
    endif()

    add_compile_definitions(-D_CRT_SECURE_NO_WARNINGS=1)
endif()
";

        assert!(!is_diff_applied_with_config(base, &diff, &config));
        match apply_bytes_reporting(base, &diff, &config) {
            ApplyOutcome::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_apply_bytes_reporting_outcomes() {
        let patch = "\
--- a/version
+++ b/version
@@ -1,3 +1,3 @@
 line 1
-3.1
+3.12
 line 3
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"line 1\n3.1\nline 3\n";
        let post = b"line 1\n3.12\nline 3\n";

        // Fresh content: applied and changed.
        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, stats) => {
                assert_eq!(content, post);
                assert!(stats.has_changes());
            }
            other => panic!("expected Applied, got {other:?}"),
        }

        // Already-patched content: reported as already applied (not a no-op
        // "Applied", and not "Failed").
        match apply_bytes_reporting(post, &diff, &config) {
            ApplyOutcome::AlreadyApplied(content) => assert_eq!(content, post),
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }

        // Unrelated content: cannot apply and is not already applied.
        match apply_bytes_reporting(b"totally different\n", &diff, &config) {
            ApplyOutcome::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn test_trailing_newline_only_diff_is_not_already_applied() {
        // Found by the GNU compat suite: a diff whose only change is adding
        // the trailing newline was judged already-applied (content-only
        // post-image matching) and skipped.
        let patch = "\
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 alpha
 beta
-gamma
\\ No newline at end of file
+gamma
";
        let diff = Diff::from_bytes(patch.as_bytes()).unwrap();
        let config = fuzzy_config();

        let pre = b"alpha\nbeta\ngamma";
        let post = b"alpha\nbeta\ngamma\n";

        assert!(!is_diff_applied_with_config(pre, &diff, &config));
        assert!(is_diff_applied_with_config(post, &diff, &config));

        match apply_bytes_reporting(pre, &diff, &config) {
            ApplyOutcome::Applied(content, _) => assert_eq!(content, post),
            other => panic!("expected Applied, got {other:?}"),
        }
        match apply_bytes_reporting(post, &diff, &config) {
            ApplyOutcome::AlreadyApplied(content) => assert_eq!(content, post),
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
    }

    #[test]
    fn test_apply_reporting_str() {
        // The str twin behaves like the bytes version.
        let patch = "\
--- a/version
+++ b/version
@@ -1,3 +1,3 @@
 line 1
-3.1
+3.12
 line 3
";
        let diff = Diff::from_str(patch).unwrap();
        let config = fuzzy_config();

        match apply_reporting("line 1\n3.1\nline 3\n", &diff, &config) {
            ApplyOutcome::Applied(content, stats) => {
                assert_eq!(content, "line 1\n3.12\nline 3\n");
                assert!(stats.has_changes());
            }
            other => panic!("expected Applied, got {other:?}"),
        }
        match apply_reporting("line 1\n3.12\nline 3\n", &diff, &config) {
            ApplyOutcome::AlreadyApplied(content) => {
                assert_eq!(content, "line 1\n3.12\nline 3\n")
            }
            other => panic!("expected AlreadyApplied, got {other:?}"),
        }
        match apply_reporting("unrelated\n", &diff, &config) {
            ApplyOutcome::Failed(_) => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
