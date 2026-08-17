mod format;
mod parse;

pub use format::PatchFormatter;
pub use parse::{HunkRangeStrategy, ParsePatchError, ParsePatchErrorKind, ParserConfig};

use std::{
    borrow::Cow,
    fmt::{self, Debug},
    ops,
};

use crate::{LineEnd, utils::Text};

const NO_NEWLINE_AT_EOF: &str = "\\ No newline at end of file";

/// An ordered collection of per-file [`Diff`]s parsed from one patch file.
///
/// This is what [`Patch::from_str`] / [`Patch::from_bytes`] (and the
/// `patch_from_*` free functions) return for patches that may touch several
/// files. It dereferences to a slice of [`Diff`]s, so indexing, iteration and
/// slice methods work directly:
///
/// ```
/// use flickzeug::Patch;
///
/// let patch = Patch::from_str(
///     "--- a/one\n+++ b/one\n@@ -1 +1 @@\n-a\n+b\n\
///      --- a/two\n+++ b/two\n@@ -1 +1 @@\n-c\n+d\n",
/// )
/// .unwrap();
///
/// assert_eq!(patch.len(), 2);
/// assert_eq!(patch[0].modified(), Some("one"));
/// for diff in &patch {
///     assert_eq!(diff.hunks().len(), 1);
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct Patch<'a, T: ToOwned + ?Sized> {
    diffs: Vec<Diff<'a, T>>,
}

impl<'a, T: ToOwned + ?Sized> Patch<'a, T> {
    /// The per-file diffs, in the order they appear in the patch
    pub fn diffs(&self) -> &[Diff<'a, T>] {
        &self.diffs
    }

    /// Consume the patch, returning its per-file diffs
    pub fn into_diffs(self) -> Vec<Diff<'a, T>> {
        self.diffs
    }
}

impl<'a> Patch<'a, str> {
    /// Parse a (potentially multi-file) patch from a string
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &'a str) -> Result<Self, ParsePatchError> {
        patch_from_str(s)
    }

    /// Parse a (potentially multi-file) patch from a string with a custom
    /// parser configuration
    pub fn from_str_with_config(s: &'a str, config: ParserConfig) -> Result<Self, ParsePatchError> {
        patch_from_str_with_config(s, config)
    }
}

impl<'a> Patch<'a, [u8]> {
    /// Parse a (potentially multi-file) patch from bytes
    pub fn from_bytes(s: &'a [u8]) -> Result<Self, ParsePatchError> {
        patch_from_bytes(s)
    }

    /// Parse a (potentially multi-file) patch from bytes with a custom parser
    /// configuration
    pub fn from_bytes_with_config(
        s: &'a [u8],
        config: ParserConfig,
    ) -> Result<Self, ParsePatchError> {
        patch_from_bytes_with_config(s, config)
    }
}

impl<T: AsRef<[u8]> + ToOwned + ?Sized> Patch<'_, T> {
    /// Convert the patch into bytes in the unified format
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for diff in &self.diffs {
            PatchFormatter::new()
                .write_patch_into(diff, &mut bytes)
                .unwrap();
        }
        bytes
    }
}

impl<'a, T: ToOwned + ?Sized> From<Vec<Diff<'a, T>>> for Patch<'a, T> {
    fn from(diffs: Vec<Diff<'a, T>>) -> Self {
        Self { diffs }
    }
}

impl<'a, T: ToOwned + ?Sized> ops::Deref for Patch<'a, T> {
    type Target = [Diff<'a, T>];

    fn deref(&self) -> &Self::Target {
        &self.diffs
    }
}

impl<'a, T: ToOwned + ?Sized> IntoIterator for Patch<'a, T> {
    type Item = Diff<'a, T>;
    type IntoIter = std::vec::IntoIter<Diff<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.diffs.into_iter()
    }
}

impl<'p, 'a, T: ToOwned + ?Sized> IntoIterator for &'p Patch<'a, T> {
    type Item = &'p Diff<'a, T>;
    type IntoIter = std::slice::Iter<'p, Diff<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.diffs.iter()
    }
}

impl fmt::Display for Patch<'_, str> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for diff in &self.diffs {
            write!(f, "{}", diff)?;
        }
        Ok(())
    }
}

impl<T> fmt::Debug for Patch<'_, T>
where
    T: ?Sized + ToOwned<Owned: Debug> + fmt::Debug + Text,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(&self.diffs).finish()
    }
}

/// How a [`Diff`] changes the file it refers to
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileChangeKind {
    /// An existing file is modified in place
    #[default]
    Modify,
    /// The file is newly created (git `new file mode` header or a `/dev/null`
    /// old side)
    Create,
    /// The file is deleted (git `deleted file mode` header or a `/dev/null`
    /// new side)
    Delete,
    /// The file is renamed, and possibly also modified (git `rename from` /
    /// `rename to` headers)
    Rename,
}

/// Metadata about the file-level change described by a [`Diff`], extracted
/// from git extended headers and explicit `/dev/null` file names.
///
/// For plain unified diffs without any of those markers this is
/// [`FileChangeKind::Modify`] with no modes.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub struct FileMetadata {
    /// What kind of change this diff describes
    pub kind: FileChangeKind,
    /// Unix file mode of the old file (e.g. `0o100644`), from git's
    /// `old mode` / `deleted file mode` headers
    pub old_mode: Option<u32>,
    /// Unix file mode of the new file, from git's `new mode` /
    /// `new file mode` headers
    pub new_mode: Option<u32>,
}

/// Representation of all the differences between two files
#[derive(Clone, PartialEq, PartialOrd, Ord, Eq)]
pub struct Diff<'a, T: ToOwned + ?Sized> {
    // TODO GNU patch is able to parse patches without filename headers.
    // This should be changed to an `Option` type to reflect this instead of setting this to ""
    // when they're missing
    original: Option<Filename<'a, T>>,
    modified: Option<Filename<'a, T>>,
    hunks: Vec<Hunk<'a, T>>,
    metadata: FileMetadata,
}

impl<'a, T: Text + ToOwned + ?Sized> Diff<'a, T> {
    pub(crate) fn new<O, M>(
        original: Option<O>,
        modified: Option<M>,
        hunks: Vec<Hunk<'a, T>>,
    ) -> Self
    where
        O: Into<Cow<'a, T>>,
        M: Into<Cow<'a, T>>,
    {
        let original = original.map(|o| Filename(o.into()));
        let modified = modified.map(|m| Filename(m.into()));
        Self {
            original,
            modified,
            hunks,
            metadata: FileMetadata::default(),
        }
    }

    pub(crate) fn with_metadata(mut self, metadata: FileMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Metadata about the file-level change (create/delete/rename, file
    /// modes) extracted from git extended headers and `/dev/null` sides
    pub fn metadata(&self) -> &FileMetadata {
        &self.metadata
    }

    /// Return the name of the old file
    ///
    /// This includes the `a/` prefix if present.
    pub fn original(&self) -> Option<&T> {
        self.original.as_ref().map(AsRef::as_ref)
    }

    /// Return the name of the new file
    ///
    /// This includes the `b/` prefix if present.
    pub fn modified(&self) -> Option<&T> {
        self.modified.as_ref().map(AsRef::as_ref)
    }

    /// Returns the hunks in the patch
    pub fn hunks(&self) -> &[Hunk<'_, T>] {
        &self.hunks
    }

    pub fn reverse(&self) -> Diff<'_, T> {
        let hunks = self.hunks.iter().map(Hunk::reverse).collect();
        let mut metadata = FileMetadata {
            kind: match self.metadata.kind {
                FileChangeKind::Create => FileChangeKind::Delete,
                FileChangeKind::Delete => FileChangeKind::Create,
                kind => kind,
            },
            ..FileMetadata::default()
        };
        metadata.old_mode = self.metadata.new_mode;
        metadata.new_mode = self.metadata.old_mode;
        Diff {
            original: self.modified.clone(),
            modified: self.original.clone(),
            hunks,
            metadata,
        }
    }
}

impl<T: AsRef<[u8]> + ToOwned + ?Sized> Diff<'_, T> {
    /// Convert a `Patch` into bytes
    ///
    /// This is the equivalent of the `to_string` function but for
    /// potentially non-utf8 patches.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        PatchFormatter::new()
            .write_patch_into(self, &mut bytes)
            .unwrap();
        bytes
    }
}

pub fn patch_from_str(input: &str) -> Result<Patch<'_, str>, ParsePatchError> {
    parse::parse_multiple(input).map(Patch::from)
}

pub fn patch_from_str_with_config(
    input: &str,
    config: ParserConfig,
) -> Result<Patch<'_, str>, ParsePatchError> {
    parse::parse_multiple_with_config(input, config).map(Patch::from)
}

pub fn patch_from_bytes(input: &[u8]) -> Result<Patch<'_, [u8]>, ParsePatchError> {
    parse::parse_bytes_multiple(input).map(Patch::from)
}

pub fn patch_from_bytes_with_config(
    input: &[u8],
    config: ParserConfig,
) -> Result<Patch<'_, [u8]>, ParsePatchError> {
    parse::parse_bytes_multiple_with_config(input, config).map(Patch::from)
}

impl<'a> Diff<'a, str> {
    /// Parse a `Patch` from a string
    ///
    /// ```
    /// use flickzeug::Diff;
    ///
    /// let s = "\
    /// --- a/ideals
    /// +++ b/ideals
    /// @@ -1,4 +1,6 @@
    ///  First:
    ///      Life before death,
    ///      strength before weakness,
    ///      journey before destination.
    /// +Second:
    /// +    I will protect those who cannot protect themselves.
    /// ";
    ///
    /// let patch = Diff::from_str(s).unwrap();
    /// assert_eq!(patch.original(), Some("ideals"));
    /// assert_eq!(patch.hunks().len(), 1);
    /// assert_eq!(patch.hunks()[0].lines().len(), 6);
    /// ```
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &'a str) -> Result<Diff<'a, str>, ParsePatchError> {
        parse::parse(s)
    }
}

impl<'a> Diff<'a, [u8]> {
    /// Parse a `Patch` from bytes
    pub fn from_bytes(s: &'a [u8]) -> Result<Diff<'a, [u8]>, ParsePatchError> {
        parse::parse_bytes(s)
    }
}

impl fmt::Display for Diff<'_, str> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", PatchFormatter::new().fmt_patch(self))
    }
}

impl<T> fmt::Debug for Diff<'_, T>
where
    T: ?Sized + ToOwned<Owned: Debug> + fmt::Debug + Text,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Patch");
        s.field("original", &self.original)
            .field("modified", &self.modified);
        // Keep the default (a plain modification) out of the debug output so
        // it stays focused on what the diff actually says
        if self.metadata != FileMetadata::default() {
            s.field("metadata", &self.metadata);
        }
        s.field("hunks", &self.hunks).finish()
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Filename<'a, T: ToOwned + ?Sized>(Cow<'a, T>);

const ESCAPED_CHARS: &[char] = &['\n', '\t', '\0', '\r', '\"', '\\'];
#[allow(clippy::byte_char_slices)]
const ESCAPED_CHARS_BYTES: &[u8] = &[b'\n', b'\t', b'\0', b'\r', b'\"', b'\\'];

impl Filename<'_, str> {
    fn needs_to_be_escaped(&self) -> bool {
        self.0.contains(ESCAPED_CHARS)
    }
}

impl<T: ToOwned + AsRef<[u8]> + ?Sized> Filename<'_, T> {
    fn needs_to_be_escaped_bytes(&self) -> bool {
        self.0
            .as_ref()
            .as_ref()
            .iter()
            .any(|b| ESCAPED_CHARS_BYTES.contains(b))
    }

    fn write_into<W: std::io::Write>(&self, mut w: W) -> std::io::Result<()> {
        if self.needs_to_be_escaped_bytes() {
            w.write_all(b"\"")?;
            for b in self.0.as_ref().as_ref() {
                if ESCAPED_CHARS_BYTES.contains(b) {
                    w.write_all(b"\\")?;
                }
                w.write_all(&[*b])?;
            }
            w.write_all(b"\"")?;
        } else {
            w.write_all(self.0.as_ref().as_ref())?;
        }

        Ok(())
    }
}

impl<T: ToOwned + ?Sized> AsRef<T> for Filename<'_, T> {
    fn as_ref(&self) -> &T {
        &self.0
    }
}

impl<T: ToOwned + ?Sized> ops::Deref for Filename<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: ToOwned + ?Sized> Clone for Filename<'_, T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl fmt::Display for Filename<'_, str> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use std::fmt::Write;
        if self.needs_to_be_escaped() {
            f.write_char('\"')?;
            for c in self.0.chars() {
                if ESCAPED_CHARS.contains(&c) {
                    f.write_char('\\')?;
                }
                f.write_char(c)?;
            }
            f.write_char('\"')?;
        } else {
            f.write_str(&self.0)?;
        }

        Ok(())
    }
}

impl<T> fmt::Debug for Filename<'_, T>
where
    T: Debug + ToOwned<Owned: Debug> + ?Sized + Text,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let internal_value = self.0.as_ref();
        let probably_readable = internal_value
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("{:#?}", internal_value));
        f.debug_tuple("Filename").field(&probably_readable).finish()
    }
}

/// Represents a group of differing lines between two files
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hunk<'a, T: ?Sized + ToOwned> {
    old_range: HunkRange,
    new_range: HunkRange,

    function_context: Option<(&'a T, Option<LineEnd>)>,

    lines: Vec<Line<'a, T>>,
}

impl fmt::Display for Hunk<'_, str> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        PatchFormatter::new().fmt_hunk(self).fmt(f)
    }
}

impl fmt::Display for Hunk<'_, [u8]> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = Vec::new();
        PatchFormatter::new()
            .write_hunk_into(self, &mut buf)
            .map_err(|_| fmt::Error)?;
        // Convert to string, replacing invalid UTF-8 with replacement character
        let s = String::from_utf8_lossy(&buf);
        write!(f, "{}", s)
    }
}

// We implement this trait manually, because we want specific type
// constraints.
impl<T> Debug for Hunk<'_, T>
where
    T: Debug + ToOwned + ?Sized + Text,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut fmt = f.debug_struct("Hunk");
        fmt.field("old_range", &self.old_range);
        fmt.field("new_range", &self.new_range);

        if let Some((line, ending)) = self.function_context {
            let mut l = line.as_bytes().to_owned();
            let e: &[u8] = ending.map(|e| e.into()).unwrap_or(&[]);
            l.extend(e);

            if let Ok(s) = std::str::from_utf8(l.as_slice()) {
                fmt.field("function_context", &Some(s));
            } else {
                fmt.field("function_context", &Some(l));
            }
        } else {
            fmt.field("function_context", &None::<&str>);
        }

        fmt.field("lines", &self.lines).finish()
    }
}

fn hunk_lines_count<T: ?Sized + ToOwned>(lines: &[Line<'_, T>]) -> (usize, usize) {
    lines.iter().fold((0, 0), |count, line| match line {
        Line::Context(_) => (count.0 + 1, count.1 + 1),
        Line::Delete(_) => (count.0 + 1, count.1),
        Line::Insert(_) => (count.0, count.1 + 1),
    })
}

impl<'a, T: Text + ?Sized + ToOwned> Hunk<'a, T> {
    pub(crate) fn new(
        old_range: HunkRange,
        new_range: HunkRange,
        function_context: Option<(&'a T, Option<LineEnd>)>,
        lines: Vec<Line<'a, T>>,
    ) -> Self {
        Self {
            old_range,
            new_range,
            function_context,
            lines,
        }
    }

    /// Returns the corresponding range for the old file in the hunk
    pub fn old_range(&self) -> HunkRange {
        self.old_range
    }

    /// Returns the corresponding range for the new file in the hunk
    pub fn new_range(&self) -> HunkRange {
        self.new_range
    }

    /// Returns the function context (if any) for the hunk
    pub fn function_context(&self) -> Option<(&T, Option<LineEnd>)> {
        self.function_context
    }

    /// Returns the lines in the hunk
    pub fn lines(&self) -> &[Line<'a, T>] {
        &self.lines
    }

    /// Creates a reverse patch for the hunk.  This is equivalent to what
    /// XDL_PATCH_REVERSE would apply in libxdiff.
    pub fn reverse(&self) -> Self {
        let lines = self.lines.iter().map(Line::reverse).collect();
        Self {
            old_range: self.new_range,
            new_range: self.old_range,
            function_context: self.function_context,
            lines,
        }
    }
}

/// The range of lines in a file for a particular `Hunk`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HunkRange {
    /// The starting line number of a hunk
    start: usize,
    /// The hunk size (number of lines)
    len: usize,
}

impl HunkRange {
    pub(crate) fn new(start: usize, len: usize) -> Self {
        Self { start, len }
    }

    /// Returns the range as a `ops::Range`
    pub fn range(&self) -> ops::Range<usize> {
        self.start..self.end()
    }

    /// Returns the starting line number of the range (inclusive)
    pub fn start(&self) -> usize {
        self.start
    }

    /// Returns the ending line number of the range (exclusive)
    pub fn end(&self) -> usize {
        self.start + self.len
    }

    /// Returns the number of lines in the range
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the range is empty (has a length of `0`)
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Display for HunkRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.start)?;
        if self.len != 1 {
            write!(f, ",{}", self.len)?;
        }
        Ok(())
    }
}

/// A line in either the old file, new file, or both.
///
/// A `Line` contains the terminating newline character `\n` unless it is the final
/// line in the file and the file does not end with a newline character.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Line<'a, T: ?Sized> {
    /// A line providing context in the diff which is present in both the old and new file
    Context((&'a T, Option<LineEnd>)),
    /// A line deleted from the old file
    Delete((&'a T, Option<LineEnd>)),
    /// A line inserted to the new file
    Insert((&'a T, Option<LineEnd>)),
}

// We want to have strings in the output whenever possible.
impl<T: ?Sized + Text> fmt::Debug for Line<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Line::Context((line, ending)) => {
                let mut l = line.as_bytes().to_owned();
                let e: &[u8] = ending.map(|e| e.into()).unwrap_or(&[]);
                l.extend(e);

                if let Ok(s) = std::str::from_utf8(l.as_slice()) {
                    f.debug_tuple("Context").field(&s).finish()
                } else {
                    f.debug_tuple("Context").field(&l).finish()
                }
            }
            Line::Delete((line, ending)) => {
                let mut l = line.as_bytes().to_owned();
                let e: &[u8] = ending.map(|e| e.into()).unwrap_or(&[]);
                l.extend(e);

                if let Ok(s) = std::str::from_utf8(l.as_slice()) {
                    f.debug_tuple("Delete").field(&s).finish()
                } else {
                    f.debug_tuple("Delete").field(&l).finish()
                }
            }
            Line::Insert((line, ending)) => {
                let mut l = line.as_bytes().to_owned();
                let e: &[u8] = ending.map(|e| e.into()).unwrap_or(&[]);
                l.extend(e);

                if let Ok(s) = std::str::from_utf8(l.as_slice()) {
                    f.debug_tuple("Insert").field(&s).finish()
                } else {
                    f.debug_tuple("Insert").field(&l).finish()
                }
            }
        }
    }
}

impl<T: ?Sized> Line<'_, T> {
    pub fn reverse(&self) -> Self {
        match *self {
            Line::Context(l) => Line::Context(l),
            Line::Delete(l) => Line::Insert(l),
            Line::Insert(l) => Line::Delete(l),
        }
    }

    pub fn line_end(&self) -> Option<LineEnd> {
        match self {
            Line::Context(l) => l.1,
            Line::Delete(l) => l.1,
            Line::Insert(l) => l.1,
        }
    }
}
