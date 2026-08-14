use super::{Diff, Hunk, Line, NO_NEWLINE_AT_EOF};
#[cfg(feature = "color")]
use nu_ansi_term::{Color, Style};
use std::{
    fmt::{Display, Formatter, Result},
    io,
};

/// Struct used to adjust the formatting of a `Patch`
#[derive(Debug)]
pub struct PatchFormatter {
    #[cfg_attr(not(feature = "color"), allow(dead_code))]
    with_color: bool,
    with_missing_newline_message: bool,
    suppress_blank_empty: bool,

    #[cfg(feature = "color")]
    styles: Styles,
}

#[cfg(feature = "color")]
#[derive(Debug)]
struct Styles {
    context: Style,
    delete: Style,
    insert: Style,
    hunk_header: Style,
    patch_header: Style,
    function_context: Style,
}

impl PatchFormatter {
    /// Construct a new formatter
    pub fn new() -> Self {
        Self {
            with_color: false,
            with_missing_newline_message: true,

            // git-diff and GNU diff print a space before empty context
            // lines; suppressing it is opt-in (diff.suppressBlankEmpty)
            suppress_blank_empty: false,

            #[cfg(feature = "color")]
            styles: Styles {
                context: Style::new(),
                delete: Color::Red.normal(),
                insert: Color::Green.normal(),
                hunk_header: Color::Cyan.normal(),
                patch_header: Style::new().bold(),
                function_context: Style::new(),
            },
        }
    }

    /// Enable formatting a patch with color
    #[cfg(feature = "color")]
    pub fn with_color(mut self) -> Self {
        self.with_color = true;
        self
    }

    /// Sets whether to format a patch with a "No newline at end of file" message.
    ///
    /// Default is `true`.
    ///
    /// Note: If this is disabled by setting to `false`, formatted patches will no longer contain
    /// sufficient information to determine if a file ended with a newline character (`\n`) or not
    /// and the patch will be formatted as if both the original and modified files ended with a
    /// newline character (`\n`).
    pub fn missing_newline_message(mut self, enable: bool) -> Self {
        self.with_missing_newline_message = enable;
        self
    }

    /// Sets whether to suppress printing of a space before empty lines.
    ///
    /// Defaults to `false`, matching git-diff and GNU diff.
    ///
    /// For more information you can refer to the [Omitting trailing blanks] manual page of GNU
    /// diff or the [diff.suppressBlankEmpty] config for `git-diff`.
    ///
    /// [Omitting trailing blanks]: https://www.gnu.org/software/diffutils/manual/html_node/Trailing-Blanks.html
    /// [diff.suppressBlankEmpty]: https://git-scm.com/docs/git-diff#Documentation/git-diff.txt-codediffsuppressBlankEmptycode
    pub fn suppress_blank_empty(mut self, enable: bool) -> Self {
        self.suppress_blank_empty = enable;
        self
    }

    /// Returns a `Display` impl which can be used to print a Patch
    pub fn fmt_patch<'a>(&'a self, patch: &'a Diff<'a, str>) -> impl Display + 'a {
        PatchDisplay { f: self, patch }
    }

    pub fn write_patch_into<T: ToOwned + AsRef<[u8]> + ?Sized, W: io::Write>(
        &self,
        patch: &Diff<'_, T>,
        w: W,
    ) -> io::Result<()> {
        PatchDisplay { f: self, patch }.write_into(w)
    }

    /// Returns a `Display` impl which can be used to print a Hunk
    pub fn fmt_hunk<'a>(&'a self, hunk: &'a Hunk<'a, str>) -> impl Display + 'a {
        HunkDisplay { f: self, hunk }
    }

    /// Write a hunk into a writer
    pub fn write_hunk_into<T: AsRef<[u8]> + ?Sized + ToOwned, W: io::Write>(
        &self,
        hunk: &Hunk<'_, T>,
        w: W,
    ) -> io::Result<()> {
        HunkDisplay { f: self, hunk }.write_into(w)
    }

    fn fmt_line<'a>(&'a self, line: &'a Line<'a, str>) -> impl Display + 'a {
        LineDisplay { f: self, line }
    }

    fn write_line_into<T: AsRef<[u8]> + ?Sized + ToOwned, W: io::Write>(
        &self,
        line: &Line<'_, T>,
        w: W,
    ) -> io::Result<()> {
        LineDisplay { f: self, line }.write_into(w)
    }
}

impl Default for PatchFormatter {
    fn default() -> Self {
        Self::new()
    }
}

/// The style roles used while formatting a patch, so the color handling can
/// live in one feature-gated place.
#[derive(Copy, Clone)]
enum StyleKind {
    Context,
    Delete,
    Insert,
    HunkHeader,
    PatchHeader,
    FunctionContext,
}

impl PatchFormatter {
    /// The ANSI prefix for `kind`, or an empty string when color is disabled
    /// (or the `color` feature is compiled out).
    fn style_prefix(&self, kind: StyleKind) -> impl Display + '_ {
        StyleAffix {
            f: self,
            kind,
            prefix: true,
        }
    }

    /// The ANSI suffix for `kind`, or an empty string when color is disabled
    /// (or the `color` feature is compiled out).
    fn style_suffix(&self, kind: StyleKind) -> impl Display + '_ {
        StyleAffix {
            f: self,
            kind,
            prefix: false,
        }
    }
}

#[cfg_attr(not(feature = "color"), allow(dead_code))]
struct StyleAffix<'a> {
    f: &'a PatchFormatter,
    kind: StyleKind,
    prefix: bool,
}

impl Display for StyleAffix<'_> {
    #[cfg_attr(not(feature = "color"), allow(unused_variables))]
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result {
        #[cfg(feature = "color")]
        if self.f.with_color {
            let style = match self.kind {
                StyleKind::Context => &self.f.styles.context,
                StyleKind::Delete => &self.f.styles.delete,
                StyleKind::Insert => &self.f.styles.insert,
                StyleKind::HunkHeader => &self.f.styles.hunk_header,
                StyleKind::PatchHeader => &self.f.styles.patch_header,
                StyleKind::FunctionContext => &self.f.styles.function_context,
            };
            if self.prefix {
                write!(fmt, "{}", style.prefix())?;
            } else {
                write!(fmt, "{}", style.suffix())?;
            }
        }
        Ok(())
    }
}

struct PatchDisplay<'a, T: ToOwned + ?Sized> {
    f: &'a PatchFormatter,
    patch: &'a Diff<'a, T>,
}

impl<T: ToOwned + AsRef<[u8]> + ?Sized> PatchDisplay<'_, T> {
    fn write_into<W: io::Write>(&self, mut w: W) -> io::Result<()> {
        if self.patch.original.is_some() || self.patch.modified.is_some() {
            write!(w, "{}", self.f.style_prefix(StyleKind::PatchHeader))?;
            if let Some(original) = &self.patch.original {
                write!(w, "--- ")?;
                original.write_into(&mut w)?;
                writeln!(w)?;
            }
            if let Some(modified) = &self.patch.modified {
                write!(w, "+++ ")?;
                modified.write_into(&mut w)?;
                writeln!(w)?;
            }
            write!(w, "{}", self.f.style_suffix(StyleKind::PatchHeader))?;
        }

        for hunk in &self.patch.hunks {
            self.f.write_hunk_into(hunk, &mut w)?;
        }

        Ok(())
    }
}

impl Display for PatchDisplay<'_, str> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        if self.patch.original.is_some() || self.patch.modified.is_some() {
            write!(f, "{}", self.f.style_prefix(StyleKind::PatchHeader))?;
            if let Some(original) = &self.patch.original {
                writeln!(f, "--- {}", original)?;
            }
            if let Some(modified) = &self.patch.modified {
                writeln!(f, "+++ {}", modified)?;
            }
            write!(f, "{}", self.f.style_suffix(StyleKind::PatchHeader))?;
        }

        for hunk in &self.patch.hunks {
            write!(f, "{}", self.f.fmt_hunk(hunk))?;
        }

        Ok(())
    }
}

struct HunkDisplay<'a, T: ?Sized + ToOwned> {
    f: &'a PatchFormatter,
    hunk: &'a Hunk<'a, T>,
}

impl<T: AsRef<[u8]> + ?Sized + ToOwned> HunkDisplay<'_, T> {
    fn write_into<W: io::Write>(&self, mut w: W) -> io::Result<()> {
        write!(w, "{}", self.f.style_prefix(StyleKind::HunkHeader))?;
        write!(w, "@@ -{} +{} @@", self.hunk.old_range, self.hunk.new_range)?;
        write!(w, "{}", self.f.style_suffix(StyleKind::HunkHeader))?;

        if let Some((ctx, _ending)) = self.hunk.function_context {
            write!(w, " ")?;
            write!(w, "{}", self.f.style_prefix(StyleKind::FunctionContext))?;
            w.write_all(ctx.as_ref())?;
            write!(w, "{}", self.f.style_suffix(StyleKind::FunctionContext))?;
        }
        writeln!(w)?;

        for line in &self.hunk.lines {
            self.f.write_line_into(line, &mut w)?;
        }

        Ok(())
    }
}

impl Display for HunkDisplay<'_, str> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        write!(f, "{}", self.f.style_prefix(StyleKind::HunkHeader))?;
        write!(f, "@@ -{} +{} @@", self.hunk.old_range, self.hunk.new_range)?;
        write!(f, "{}", self.f.style_suffix(StyleKind::HunkHeader))?;

        if let Some((ctx, _ending)) = self.hunk.function_context {
            write!(f, " ")?;
            write!(f, "{}", self.f.style_prefix(StyleKind::FunctionContext))?;
            write!(f, "{}", ctx)?;
            write!(f, "{}", self.f.style_suffix(StyleKind::FunctionContext))?;
        }
        writeln!(f)?;

        for line in &self.hunk.lines {
            write!(f, "{}", self.f.fmt_line(line))?;
        }

        Ok(())
    }
}

struct LineDisplay<'a, T: ?Sized + ToOwned> {
    f: &'a PatchFormatter,
    line: &'a Line<'a, T>,
}

impl<T: AsRef<[u8]> + ?Sized + ToOwned> LineDisplay<'_, T> {
    fn write_into<W: io::Write>(&self, mut w: W) -> io::Result<()> {
        let (sign, (line, ending), kind) = match self.line {
            Line::Context(line) => (' ', line, StyleKind::Context),
            Line::Delete(line) => ('-', line, StyleKind::Delete),
            Line::Insert(line) => ('+', line, StyleKind::Insert),
        };

        write!(w, "{}", self.f.style_prefix(kind))?;

        if self.f.suppress_blank_empty
            && sign == ' '
            && line.as_ref().is_empty()
            && ending.is_some()
        {
            w.write_all(line.as_ref())?;
            if let Some(end) = *ending {
                let e: &[u8] = end.into();
                w.write_all(e)?;
            }
        } else {
            write!(w, "{}", sign)?;
            w.write_all(line.as_ref())?;
            if let Some(end) = *ending {
                let e: &[u8] = end.into();
                w.write_all(e)?;
            }
        }

        write!(w, "{}", self.f.style_suffix(kind))?;

        if ending.is_none() {
            writeln!(w)?;
            if self.f.with_missing_newline_message {
                writeln!(w, "{}", NO_NEWLINE_AT_EOF)?;
            }
        }

        Ok(())
    }
}

impl Display for LineDisplay<'_, str> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        let (sign, (line, ending), kind) = match self.line {
            Line::Context(line) => (' ', line, StyleKind::Context),
            Line::Delete(line) => ('-', line, StyleKind::Delete),
            Line::Insert(line) => ('+', line, StyleKind::Insert),
        };

        write!(f, "{}", self.f.style_prefix(kind))?;

        if self.f.suppress_blank_empty && sign == ' ' && line.is_empty() && ending.is_some() {
            write!(f, "{}", line)?;
            if let Some(end) = *ending {
                let e: &str = end.into();
                write!(f, "{}", e)?;
            }
        } else {
            write!(f, "{}{}", sign, line)?;
            if let Some(end) = *ending {
                let e: &str = end.into();
                write!(f, "{}", e)?;
            }
        }

        write!(f, "{}", self.f.style_suffix(kind))?;

        if ending.is_none() {
            writeln!(f)?;
            if self.f.with_missing_newline_message {
                writeln!(f, "{}", NO_NEWLINE_AT_EOF)?;
            }
        }

        Ok(())
    }
}
