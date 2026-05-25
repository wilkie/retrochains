//! Driver: parse the BCC command line into a shape the rest of the
//! compiler can act on. Today we recognize only the flags needed by the
//! starter fixtures (`-S`, `-c`, `-m<memory-model>`) plus a positional
//! source file. The shape of the parser anticipates the eventual full
//! BCC surface; the surface grows fixture-by-fixture.

use std::path::PathBuf;

/// What was on the command line, post-parse.
#[derive(Debug, Clone)]
pub struct ParsedArgs {
    pub mode: CompileMode,
    pub memory_model: MemoryModel,
    /// `-d`: merge identical string literals into a single pool
    /// entry. Default off — each occurrence in source gets its own
    /// slot. Fixtures 2282 (`-d` set, dedup'd), 2283 (no flag,
    /// duplicated).
    pub merge_strings: bool,
    /// Input source files, in the order given on the command line.
    pub sources: Vec<PathBuf>,
}

/// Which compilation stage to stop at. Maps to the `-S` / `-c` family of
/// BCC flags. The default (no flag) would be "compile + assemble + link
/// to executable", which we don't yet support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompileMode {
    /// `-S`: compile to assembly, stop before the assembler.
    Assembly,
    /// `-c`: compile and assemble, stop before the linker. Not yet
    /// supported.
    Object,
}

/// `-m<x>`: which memory model to compile for. Only `-ms` (small) is
/// recognized for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryModel {
    Small,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("usage: bcc -S -ms <source>.C")]
    Usage,
    #[error("unsupported flag: {0}")]
    Unsupported(String),
    #[error("missing input file")]
    NoSource,
    #[error("compile mode not specified (need -S or -c)")]
    NoMode,
}

/// Parse a BCC argv (without argv[0]) into [`ParsedArgs`].
///
/// # Errors
/// Returns [`CliError`] for unrecognized or insufficient arguments.
pub fn parse_args(argv: &[String]) -> Result<ParsedArgs, CliError> {
    let mut mode: Option<CompileMode> = None;
    let mut memory_model: Option<MemoryModel> = None;
    let mut merge_strings = false;
    let mut sources: Vec<PathBuf> = Vec::new();
    for arg in argv {
        match arg.as_str() {
            "-S" => mode = Some(CompileMode::Assembly),
            "-c" => mode = Some(CompileMode::Object),
            "-ms" => memory_model = Some(MemoryModel::Small),
            "-d" => merge_strings = true,
            other if other.starts_with('-') => {
                return Err(CliError::Unsupported(other.to_owned()));
            }
            path => sources.push(PathBuf::from(path)),
        }
    }
    let mode = mode.ok_or(CliError::NoMode)?;
    let memory_model = memory_model.unwrap_or(MemoryModel::Small);
    if sources.is_empty() {
        return Err(CliError::NoSource);
    }
    Ok(ParsedArgs { mode, memory_model, merge_strings, sources })
}
