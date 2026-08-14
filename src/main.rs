//! Standalone command line tools built on the flickzeug library.
//!
//! Built only when the `cli` feature is enabled:
//!
//! ```console
//! cargo install flickzeug --features cli
//! ```

use std::{
    fs, io,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::ExitCode,
};

use clap::{Parser, Subcommand, ValueEnum};
use flickzeug::{
    ApplyConfig, ApplyOutcome, ConflictStyle, DiffOptions, FuzzyConfig, HunkRangeStrategy,
    MergeOptions, ParserConfig, PatchFormatter, apply_bytes_reporting,
    patch_from_bytes_with_config,
};

/// diff, patch and merge tools based on the flickzeug library
#[derive(Parser)]
#[command(name = "flickzeug", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compare two files and print a unified diff.
    ///
    /// Exits with 0 if the files are identical, 1 if they differ and 2 on
    /// trouble (mirroring GNU diff).
    Diff {
        /// The original file
        original: PathBuf,
        /// The modified file
        modified: PathBuf,
        /// Number of context lines around each change
        #[arg(short = 'U', long = "unified", default_value_t = 3)]
        context: usize,
        /// Colorize the output
        #[arg(long)]
        color: bool,
    },
    /// Apply a patch in unified diff format to files in a directory.
    ///
    /// Hunks are located with the same fuzzy matching the library uses in
    /// production; patches that are already applied are detected and skipped.
    /// Exits with 0 on success (including already-applied patches), 1 if any
    /// file failed to patch and 2 on trouble.
    Apply {
        /// The patch file to apply (reads stdin when omitted or "-")
        patch: Option<PathBuf>,
        /// Directory the file names in the patch are resolved against
        #[arg(short = 'd', long, default_value = ".")]
        directory: PathBuf,
        /// Strip this many additional leading path components (the
        /// conventional a/ and b/ prefixes are always stripped)
        #[arg(short = 'p', long, default_value_t = 0)]
        strip: usize,
        /// Maximum fuzz: context lines that may be ignored at hunk edges
        #[arg(short = 'F', long, default_value_t = 2)]
        fuzz: usize,
        /// Apply the patch in reverse
        #[arg(short = 'R', long)]
        reverse: bool,
        /// Parse leniently: recount hunk ranges and ignore hunk ordering
        #[arg(long)]
        lenient: bool,
        /// Report what would happen without modifying any files
        #[arg(long)]
        dry_run: bool,
    },
    /// Three-way merge of two files with a common ancestor.
    ///
    /// Argument order matches `git merge-file`: ours base theirs. Prints the
    /// merged result and exits with 0 on a clean merge, 1 when there are
    /// conflicts (the output then contains conflict markers) and 2 on trouble.
    Merge {
        /// Our version of the file
        ours: PathBuf,
        /// The common ancestor
        base: PathBuf,
        /// Their version of the file
        theirs: PathBuf,
        /// Write the result to this file instead of stdout
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        /// Conflict marker style
        #[arg(long, value_enum, default_value_t = StyleArg::Diff3)]
        style: StyleArg,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum StyleArg {
    /// Only ours/theirs conflict markers
    Merge,
    /// Include the original lines between ||||||| and =======
    Diff3,
}

impl From<StyleArg> for ConflictStyle {
    fn from(style: StyleArg) -> Self {
        match style {
            StyleArg::Merge => ConflictStyle::Merge,
            StyleArg::Diff3 => ConflictStyle::Diff3,
        }
    }
}

type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Diff {
            original,
            modified,
            context,
            color,
        } => run_diff(&original, &modified, context, color),
        Command::Apply {
            patch,
            directory,
            strip,
            fuzz,
            reverse,
            lenient,
            dry_run,
        } => run_apply(
            patch.as_deref(),
            &directory,
            strip,
            fuzz,
            reverse,
            lenient,
            dry_run,
        ),
        Command::Merge {
            ours,
            base,
            theirs,
            output,
            style,
        } => run_merge(&ours, &base, &theirs, output.as_deref(), style.into()),
    };

    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("flickzeug: {err}");
            ExitCode::from(2)
        }
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|err| format!("{}: {err}", path.display()).into())
}

fn run_diff(
    original: &Path,
    modified: &Path,
    context: usize,
    color: bool,
) -> Result<ExitCode, Error> {
    let original_data = read_file(original)?;
    let modified_data = read_file(modified)?;

    let mut options = DiffOptions::new();
    options
        .set_context_len(context)
        .set_original_filename(original.display().to_string())
        .set_modified_filename(modified.display().to_string());

    let patch = options.create_patch_bytes(&original_data, &modified_data);
    if patch.hunks().is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    let formatter = if color {
        PatchFormatter::new().with_color()
    } else {
        PatchFormatter::new()
    };
    formatter.write_patch_into(&patch, io::stdout().lock())?;

    Ok(ExitCode::from(1))
}

/// Strip `n` leading components and reject paths that could escape `directory`.
fn resolve_target(directory: &Path, name: &[u8], strip: usize) -> Result<PathBuf, Error> {
    let name =
        String::from_utf8(name.to_vec()).map_err(|_| "patch contains a non-utf8 file name")?;
    let path: PathBuf = Path::new(&name).components().skip(strip).collect();

    if path.as_os_str().is_empty() {
        return Err(format!("nothing left of file name {name:?} after -p{strip}").into());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(format!("refusing to touch unsafe path {name:?}").into()),
        }
    }

    Ok(directory.join(path))
}

fn run_apply(
    patch: Option<&Path>,
    directory: &Path,
    strip: usize,
    fuzz: usize,
    reverse: bool,
    lenient: bool,
    dry_run: bool,
) -> Result<ExitCode, Error> {
    let patch_data = match patch {
        Some(path) if path.as_os_str() != "-" => read_file(path)?,
        _ => {
            let mut buffer = Vec::new();
            io::stdin().lock().read_to_end(&mut buffer)?;
            buffer
        }
    };

    let parser_config = ParserConfig {
        hunk_strategy: if lenient {
            HunkRangeStrategy::Recount
        } else {
            HunkRangeStrategy::Check
        },
        skip_order_check: lenient,
        strip_ab_prefix: true,
    };
    let diffs = patch_from_bytes_with_config(&patch_data, parser_config)
        .map_err(|err| format!("failed to parse patch: {err}"))?;
    if diffs.is_empty() {
        return Err("patch contains no file diffs".into());
    }

    let apply_config = ApplyConfig {
        fuzzy_config: FuzzyConfig {
            max_fuzz: fuzz,
            ..FuzzyConfig::default()
        },
        ..ApplyConfig::default()
    };

    let mut failures = 0usize;
    for diff in &diffs {
        let reversed;
        let diff = if reverse {
            reversed = diff.reverse();
            &reversed
        } else {
            diff
        };

        let original = diff
            .original()
            .map(|name| resolve_target(directory, name, strip))
            .transpose()?;
        let modified = diff
            .modified()
            .map(|name| resolve_target(directory, name, strip))
            .transpose()?;

        match (original, modified) {
            (None, None) => {}
            // Pure rename or metadata-only diff without hunks
            (Some(from), Some(to)) if diff.hunks().is_empty() => {
                if from == to {
                    println!("{}: no content changes, skipped", from.display());
                } else {
                    println!("renaming {} to {}", from.display(), to.display());
                    if !dry_run {
                        rename_file(&from, &to)?;
                    }
                }
            }
            // File deletion: apply the hunks and remove the file if empty
            (Some(from), None) => {
                let base = read_file(&from)?;
                match apply_bytes_reporting(&base, diff, &apply_config) {
                    ApplyOutcome::Applied(content, _) if content.is_empty() => {
                        println!("removing file {}", from.display());
                        if !dry_run {
                            fs::remove_file(&from)?;
                        }
                    }
                    ApplyOutcome::Applied(content, _) => {
                        println!("patching file {} (not removed: not empty)", from.display());
                        if !dry_run {
                            fs::write(&from, content)?;
                        }
                    }
                    ApplyOutcome::AlreadyApplied(_) => {
                        println!("{}: already applied, skipped", from.display());
                    }
                    ApplyOutcome::Failed(err) => {
                        eprintln!("{}: {err}", from.display());
                        failures += 1;
                    }
                }
            }
            // File creation or modification (possibly a rename)
            (original, Some(to)) => {
                let base = match &original {
                    Some(from) if from.exists() => read_file(from)?,
                    Some(_) | None if to.exists() => read_file(&to)?,
                    _ => Vec::new(),
                };
                match apply_bytes_reporting(&base, diff, &apply_config) {
                    ApplyOutcome::Applied(content, _) => {
                        println!("patching file {}", to.display());
                        if !dry_run {
                            if let Some(parent) = to.parent() {
                                fs::create_dir_all(parent)?;
                            }
                            fs::write(&to, content)?;
                            if let Some(from) = &original
                                && *from != to
                                && from.exists()
                            {
                                fs::remove_file(from)?;
                            }
                        }
                    }
                    ApplyOutcome::AlreadyApplied(_) => {
                        println!("{}: already applied, skipped", to.display());
                    }
                    ApplyOutcome::Failed(err) => {
                        eprintln!("{}: {err}", to.display());
                        failures += 1;
                    }
                }
            }
        }
    }

    if failures > 0 {
        eprintln!("{failures} out of {} file(s) failed to patch", diffs.len());
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn rename_file(from: &Path, to: &Path) -> Result<(), Error> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to)?;
    Ok(())
}

fn run_merge(
    ours: &Path,
    base: &Path,
    theirs: &Path,
    output: Option<&Path>,
    style: ConflictStyle,
) -> Result<ExitCode, Error> {
    let ours_data = read_file(ours)?;
    let base_data = read_file(base)?;
    let theirs_data = read_file(theirs)?;

    let mut options = MergeOptions::new();
    options.set_conflict_style(style);

    let (merged, conflicts) = match options.merge_bytes(&base_data, &ours_data, &theirs_data) {
        Ok(merged) => (merged, false),
        Err(merged) => (merged, true),
    };

    match output {
        Some(path) => fs::write(path, merged)?,
        None => io::stdout().lock().write_all(&merged)?,
    }

    if conflicts {
        eprintln!("flickzeug: merge conflicts found");
        Ok(ExitCode::from(1))
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
