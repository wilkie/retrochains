# Running Borland C++ 2.0 (the oracle)

The original 1991 Borland C++ 2.0 install tree — `BCC.EXE`, `TASM.EXE`,
`TLINK.EXE`, all the headers in `INCLUDE/`, and every memory-model variant of
the runtime libraries in `LIB/` — is stored as `BC2.zip` at the repository
root. It's the only Borland-distributed file that lives in version control;
everything else is unpacked on demand.

We run those original binaries on a modern host with [DOSBox](https://www.dosbox.com/)
and call the result *the oracle*: when we build our own implementation,
every byte we emit must match what the oracle emits for the same input.

## Layout

- `BC2.zip` — the install tree. Tracked. Read-only.
- `.bc2/` — gitignored. Lazily populated with the unpacked contents of
  `BC2.zip` the first time the oracle is used.
- `crates/oracle/` — the wrapper. Public API in `oracle::Oracle`. Handles
  unpacking, building the DOSBox command, capturing stdout, exit code, and any
  files the tool produced.

## Prerequisites

```bash
sudo apt install dosbox faketime
```

- **dosbox** — vanilla DOSBox 0.74, fine for non-graphical CLI use.
- **faketime** (libfaketime) — wraps the DOSBox process to pin the emulated
  DOS clock to a fixed instant. Without it, BCC stamps the current time into
  every OBJ, breaking byte-exact reproducibility (see "How the wrapper drives
  DOSBox" below).

DOSBox is invoked headlessly — we run it with `SDL_VIDEODRIVER=dummy` so no X
server or graphics output is required (works on a bare WSL2 host).

## Using the oracle from Rust

```rust
use oracle::{Oracle, OracleConfig, OracleInvocation, Tool};

let cfg = OracleConfig::for_workspace(workspace_root);
let oracle = Oracle::open(cfg)?;       // unpacks BC2.zip on first call

let run = oracle.run(
    &OracleInvocation::new(Tool::Bcc)
        .args(["-ms", "-c", "HELLO.C"])
        .input("HELLO.C", b"int main(void){return 0;}\n"),
)?;
assert_eq!(run.exit_code, 0);
let obj_bytes = &run.outputs["HELLO.OBJ"];
```

`OracleInvocation::input` materializes a file in the DOS-visible working
directory under the given DOS filename. `OracleInvocation::args` are passed
verbatim on the BCC/TASM/TLINK command line — so the caller is responsible for
naming the file as it'll appear after materialization (uppercase, 8.3, etc.).

## Using the oracle from the shell

There's an `oracle` binary for ad-hoc use:

```bash
# Compile-only (small memory model): produces HELLO.OBJ in cwd.
cargo run -q -p oracle -- bcc -ms -c -- hello.c
```

The `--` separator delineates `<tool args>` from `<host-side input files>`.
Each input file is read from the host, materialized in the DOS work directory
under its basename uppercased, and its DOS name is appended to the tool's
argument list. Any new files the tool produces are written back to cwd.

## Default `BCC` flags

The shipping `BCC.EXE` default flags (when invoked with no options) are the
moral equivalent of:

```
BCC -ms -p- -k -V -Z -O -r -G -ID:\BC2\INCLUDE -LD:\BC2\LIB FOO.CPP
```

Our oracle pre-sets `INCLUDE` and `LIB` via DOS env vars (`set INCLUDE=C:\INCLUDE`,
`set LIB=C:\LIB`) so callers don't need to pass `-I`/`-L` for the system tree.

## How the wrapper drives DOSBox

For each invocation, the oracle:

1. Creates a fresh temp directory on the host.
2. Materializes all caller-provided input files into it (under their DOS names).
3. Writes a `_RUN.BAT` batch file that runs the tool with stdout redirected to
   `_OUT.TXT`, then `GOTO`s to write either `0` or `1` to `_RC.TXT` depending
   on `ERRORLEVEL`, then `EXIT`s DOSBox.
4. Runs `dosbox -exit` with `-c` commands that mount the BC2 tree as `C:`, the
   temp dir as `D:`, set `PATH=C:\BIN`, `INCLUDE=C:\INCLUDE`, `LIB=C:\LIB`, and
   then call `_RUN.BAT`.
5. Reads `_RC.TXT`, `_OUT.TXT`, and every other file in the work dir as
   outputs.

A few things in there are fixed by DOSBox 0.74 quirks rather than chosen for
elegance:

- **The batch file is necessary**, not optional: DOSBox 0.74's shell applies
  `>` redirects unconditionally — even when the `IF ERRORLEVEL` guarding them
  is false — so doing the sentinel writes inline (`-c "IF ERRORLEVEL 1 ECHO 1
  > _RC.TXT"`) ends up truncating the file regardless of the condition. A
  batch with `GOTO` labels means only one branch's redirect ever runs.
- **`EXIT` belongs inside the batch**, not as a trailing `-c "exit"` on the
  DOSBox command line: subsequent `-c` commands after a `-c <batch>` are
  silently dropped, so DOSBox would hang at the DOS prompt waiting for input.
- **No `2>` for stderr.** DOSBox 0.74's shell honors `cmd … 2>FILE` as a
  redirect *but also leaves the leading `2` in the command's argv*, so BCC
  ends up trying to compile a phantom "2.CPP" alongside the real input. We
  use a single `>` (which captures stdout) and leave stderr unsplit —
  Borland tools write essentially everything to stdout anyway. The
  oracle's `OracleRun::stderr` field exists for forward-compatibility but
  is always empty under DOSBox 0.74.
- **`ORACLE_KEEP_WORKDIR=1`** in the environment leaves the temp dir on disk
  after the run, with its `_RUN.BAT`, `_OUT.TXT`, `_RC.TXT`, and any tool
  outputs visible. Useful when diagnosing what DOSBox actually did.

## Clock pinning — two layers

BCC stamps a DOS-packed timestamp into a handful of OMF records, so without
intervention the same input produces a different OBJ on every run (the
timestamp bytes change, and the OMF record checksums recompute around them).
DOSBox 0.74's own `DATE`/`TIME` commands don't propagate back into the
emulated DOS environment, so we have to attack this from the host side. The
oracle does two things on every run, both controlled by `OracleConfig::fake_time`:

### 1. Pin input-file mtimes

The single biggest source of nondeterminism is that **BCC reads the source
file's modification time** (via DOS INT 21h AH=57h, "Get File's Date/Time")
and embeds it in the OMF. That's a file-stat call, not a time syscall, so
it doesn't go through libc and `libfaketime` can't intercept it.

Before launching DOSBox the oracle materializes every caller-provided input
file in the work directory and explicitly sets its mtime to `FakeTime::instant`
(default: 1991-04-23 12:00:00 UTC). This makes byte-exact reproducibility
work without any host-level tricks. Decoding the embedded DOS-packed
timestamp from a successful run confirms the round-trip:

```
$ xxd HELLO.OBJ | sed -n 4p
00000030: 00e9 0060 9716 0768 656c 6c6f 2e63 c788  ...`...hello.c..
                     ^^^^^^^^^^^^
                     0x16970060 = 1991-04-23 12:00:00 (DOS packed)
```

### 2. Wrap DOSBox in `faketime` (defense in depth)

We also wrap the DOSBox spawn in `faketime` so any *other* time-dependent
code path — BCC reading the current DOS clock for stamps we haven't found
yet, the runtime libraries, future extensions — observes the same instant:

```
faketime -f "@1991-04-23 12:00:00" dosbox -exit -c ...
```

- The `-f` flag bypasses faketime's `date(1)`-based timestamp validation
  (which rejects the `@<date-string>` form we need).
- The leading `@` tells `libfaketime` to *freeze* time at that instant
  rather than let it advance — important because DOSBox makes many time
  calls per run and we want every one to return the same value.
- `TZ=UTC` is set in the spawned process so the timestamp string isn't
  reinterpreted in the host's local timezone.

`OracleConfig::fake_time = None` disables both layers, which you generally
shouldn't do. If you customize the instant, set both the `timestamp` string
and the `instant` `SystemTime` on `FakeTime` so they name the same moment.
