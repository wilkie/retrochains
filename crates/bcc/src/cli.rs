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
    /// `-D<NAME>` / `-D<NAME>=<body>`: preprocessor macros to
    /// pre-define before processing the source. Each entry is
    /// `(name, body)`; the body is empty for bare `-D<NAME>`.
    /// Fixtures 2131, 2280.
    pub defines: Vec<(String, String)>,
    /// `-K`: plain `char` defaults to unsigned char. Affects the
    /// widening step at char→int promotion (zero-extend via
    /// `mov ah, 0` instead of sign-extend via `cbw`). Fixtures
    /// 2130, 2284.
    pub unsigned_chars: bool,
    /// `-O` / `-O2`: enable BCC's small set of peephole
    /// optimizations. Today we honor it for the trampoline-jmp
    /// fold (drop `jmp short @X` when it's immediately followed
    /// by `@X:`). Fixtures 2125, 2126, 2281.
    pub optimize: bool,
    /// `-1` / `-2`: target the 80186 (or 80286) instruction set.
    /// Enables `enter`, `leave`, and the immediate-multi-bit
    /// shift form. We treat both flags the same since the 286
    /// extensions we care about (these three) were already in
    /// the 186. Fixtures 2134, 2276.
    pub target_186: bool,
    /// `-N`: insert a stack-overflow check at every function
    /// entry. BCC emits `cmp word ptr ___brklvl, sp; jb @skip;
    /// call near ptr N_OVERFLOW@; @skip:` right after the
    /// prologue. Fixture 2129.
    pub stack_check: bool,
    /// `-r-`: disable register variables. All locals/params stay
    /// on the stack regardless of use count. Fixture 2263.
    pub no_reg_vars: bool,
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

/// `-m<x>`: which memory model to compile for. The OBJ-level
/// difference between Tiny and Small for trivial code is just a
/// 1-byte change in the COMENT class-0xEA model marker (`08` vs
/// `09`); other models additionally change segment names and
/// near/far call/return conventions, which the rest of the
/// codegen pipeline hasn't been wired up for yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryModel {
    Tiny,
    Small,
    Compact,
    Medium,
    Large,
    Huge,
}

impl MemoryModel {
    /// True for memory models that use far code: medium, large, huge.
    /// These emit far returns (`retf`) by default and use a module-
    /// prefixed code segment name (`HELLO_TEXT` rather than `_TEXT`).
    #[must_use]
    pub fn has_far_code(self) -> bool {
        matches!(self, Self::Medium | Self::Large | Self::Huge)
    }

    /// The BCC OMF COMENT class-0xEA second-byte marker for this
    /// model. Tiny=0x08, Small=0x09, Medium=0x0A, Compact=0x0B,
    /// Large=0x0C, Huge=0x0D.
    #[must_use]
    pub fn marker_byte(self) -> u8 {
        match self {
            Self::Tiny => 0x08,
            Self::Small => 0x09,
            Self::Medium => 0x0A,
            Self::Compact => 0x0B,
            Self::Large => 0x0C,
            Self::Huge => 0x0D,
        }
    }
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
    let mut unsigned_chars = false;
    let mut optimize = false;
    let mut target_186 = false;
    let mut stack_check = false;
    let mut no_reg_vars = false;
    let mut defines: Vec<(String, String)> = Vec::new();
    let mut sources: Vec<PathBuf> = Vec::new();
    for arg in argv {
        match arg.as_str() {
            "-S" => mode = Some(CompileMode::Assembly),
            "-c" => mode = Some(CompileMode::Object),
            "-ms" => memory_model = Some(MemoryModel::Small),
            "-mt" => memory_model = Some(MemoryModel::Tiny),
            "-mc" => memory_model = Some(MemoryModel::Compact),
            "-mm" => memory_model = Some(MemoryModel::Medium),
            "-ml" => memory_model = Some(MemoryModel::Large),
            "-mh" => memory_model = Some(MemoryModel::Huge),
            "-d" => merge_strings = true,
            "-K" => unsigned_chars = true,
            "-O" | "-O2" => optimize = true,
            "-1" | "-2" => target_186 = true,
            "-N" => stack_check = true,
            "-r-" => no_reg_vars = true,
            other if other.starts_with("-D") => {
                // `-D<NAME>` or `-D<NAME>=<body>`. Fixtures 2131
                // (`-DFOO=42`), 2280 (`-DDEBUG=42`).
                let body = &other[2..];
                if let Some(eq) = body.find('=') {
                    defines.push((body[..eq].to_string(), body[eq + 1..].to_string()));
                } else {
                    defines.push((body.to_string(), String::new()));
                }
            }
            // BCC accepts a number of optimization / target / debug
            // flags that affect codegen but don't require us to do
            // anything different here. Silently accept the ones
            // we've seen in the fixture corpus so the corresponding
            // \`bcc -c\` invocation doesn't error out:
            //   -O       optimization
            //   -O2      more optimization
            //   -G       speed-favoring opt
            //   -1       186 target
            //   -2       286 target
            //   -K       unsigned char default
            //   -N       stack check
            //   -A       ANSI mode
            //   -r-      disable register allocator
            //   -f-      no FPU
            // Fixtures 2123-2137, 2261-2263.
            "-G" | "-A"
            | "-f-" | "-N-" => {}
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
    Ok(ParsedArgs {
        mode,
        memory_model,
        merge_strings,
        defines,
        unsigned_chars,
        optimize,
        target_186,
        stack_check,
        no_reg_vars,
        sources,
    })
}
