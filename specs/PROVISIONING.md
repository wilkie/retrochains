# Provisioning the oracle compilers

The original compiler binaries are **not** in this repository — they aren't ours
to redistribute. What *is* tracked, per compiler, is enough to rebuild them
byte-for-byte from public install media:

- `oracles/<c>/<NAME>.toml` — where to download the original install media, the
  media's `sha256`, and an `[install]` recipe describing how the installer
  assembles its files.
- `oracles/<c>/<NAME>.sha256` — the byte-exact manifest: the `sha256` of every
  file in the assembled toolchain.

The `oracle provision` command turns those two tracked files back into the
gitignored archive the oracle harness drives (`BC2.zip`, `MSC500.zip`), verifying
every file against the manifest along the way. This is the same byte-exact
contract the fixtures gate on, so a freshly provisioned toolchain is guaranteed
to match the one the goldens were captured against.

> **When do you need this?** `xfix verify-all` works on a fresh checkout with no
> archives at all — it gates on the recorded hashes, not on driving the real
> compiler (see [`../CLAUDE.md`](../CLAUDE.md)). You only need to provision when
> you want to *re-drive the original compiler* — i.e. `xfix capture` /
> `xfix materialize` to add fixtures or inspect a byte-level diff.

## TL;DR

```sh
cargo build --workspace --bins

target/debug/oracle provision bcc   # downloads media, assembles, verifies -> ./oracles/bcc/BC2.zip   (99 files)
target/debug/oracle provision msc   # downloads media, assembles, verifies -> ./oracles/msc/MSC500.zip (136 files)
```

Each command prints `provisioned <name> -> <archive> (<N> files verified against
the manifest)` on success, and exits non-zero (without writing the archive) if
any file fails to match.

## What provisioning produces (and what it doesn't)

The repo has **two** separate sets of gitignored, hash-pinned binaries. Keep them
straight:

- **The compiler toolchain** — `BC2.zip` / `MSC500.zip`: the original `BCC.EXE`,
  `TASM.EXE`, `LINK.EXE`, headers, runtime libraries, **and the toolchain's own
  object files** (e.g. `BC2/LIB/C0*.OBJ` startup modules and `WILDARGS.OBJ`;
  MSC's `*VARSTCK.OBJ`, `SETARGV.OBJ`, …). These `.OBJ` are part of the
  distribution, so **`oracle provision` does produce and verify them** — they're
  among the 99 / 136 files in `oracles/<c>/<NAME>.sha256`.

- **The fixture goldens** — the compiler's *per-fixture outputs*
  (`<NAME>.OBJ`/`.ASM`/`.EXE`/`.MAP` under `fixtures/.../expected/<compiler>/`).
  These are **not** produced by `oracle provision`. They're a separate
  reproducible cache, pinned by `expected/<compiler>/manifest.toml` and
  regenerated with **`xfix materialize <fixture>`**, which drives the
  *provisioned* compiler to re-emit them and asserts each matches its recorded
  hash. See [`FIXTURES.md`](FIXTURES.md).

So `oracle provision` rebuilds the **compiler** (object files and all); `xfix`
uses that compiler to (re)generate the **fixtures'** object files:

```sh
oracle provision bcc          # rebuild the BCC toolchain (incl. its startup .OBJ)
xfix materialize <fixture>    # drive that compiler to regenerate a fixture's golden .OBJ/.ASM/.EXE/.MAP
```

`xfix verify-all` needs neither archive — it checks recorded hashes directly.

## Prerequisites

| Tool | Needed for | Notes |
|------|-----------|-------|
| Rust toolchain | building `oracle` | `cargo build --workspace --bins` |
| `curl` | fetching the media | resolves the WinWorld mirror and downloads the `.7z` |
| Info-ZIP `unzip` + `zip` | **BCC** | expands the 1991 Implode/Shrink archives and reconstructs the split `CMDLINE.CAx` volume (`zip -FF`); the Rust `zip` crate can't read those legacy methods |
| **DOSBox-X** | **MSC only** | runs the vendor `LIB.EXE` to build the 12 combined floating-point libraries (the only step the MS C installer does beyond copying files) |

`curl`, `unzip`, and `zip` are present on essentially every Unix host. The 7z
media is expanded by a pure-Rust reader (no system `7z` needed).

### DOSBox-X (MSC)

The MSC provisioner invokes DOSBox-X via the `$ORACLE_DOSBOX_X` command,
defaulting to the Flathub build:

```
flatpak run --env=SDL_VIDEODRIVER=dummy --env=SDL_AUDIODRIVER=dummy com.dosbox_x.DOSBox-X
```

Install it with:

```sh
flatpak install flathub com.dosbox_x.DOSBox-X
```

If you have a native DOSBox-X instead, point the provisioner at it:

```sh
export ORACLE_DOSBOX_X=dosbox-x
```

(The value is split on whitespace, so you can pass flags too. The provisioner
appends `-silent -exit` and the mount/run commands; it always sets the dummy SDL
drivers so the build runs headless.)

## What the pipeline does

`oracle provision <name>` runs five stages, all driven by the descriptor:

1. **fetch** — resolve the descriptor's WinWorld landing page to a download
   mirror, download the `.7z`, and gate it on the descriptor's `archive_sha256`.
   (A previously downloaded, still-matching file is reused.)
2. **unpack** — expand the `.7z` into the original install floppy images.
3. **install** — run the `[install]` recipe to assemble the toolchain tree:
   - **BCC**: expand the install archives with `unzip`; reconstruct the
     split-volume archive that holds `BCC.EXE` with `zip -FF`; relocate the
     `INCLUDE/SYS/` headers.
   - **MSC**: copy files verbatim out of the floppy images; build the 12
     combined FP libraries by running the vendor `LIB.EXE` under DOSBox-X; write
     the two files the installer generates (`NEW-VARS.BAT`, `NEW-CONF.SYS`).
4. **verify** — hash every file in the assembled tree against the `.sha256`
   manifest. **This is the correctness gate** — provisioning fails here if
   anything doesn't reproduce.
5. **repackage** — zip the verified tree into the canonical archive beside its
   descriptor (`oracles/<c>/<NAME>.zip`), which the oracle's lazy-extract path
   consumes unchanged.

Run any subcommand from anywhere inside the repo (the workspace root is located
by walking up to the `oracles/` directory).

## Commands

```sh
# Full pipeline -> canonical archive at oracles/<c>/<NAME>.zip.
oracle provision <bcc|msc>

# Just fetch + unpack the media; print the disk-image paths (no assembly).
oracle provision <bcc|msc> fetch

# Hash an already-assembled tree against the manifest (read-only).
oracle provision <bcc|msc> verify <tree-dir>

# Verify, then seal a tree into a zip (defaults to the canonical archive path).
oracle provision <bcc|msc> repackage <tree-dir> [out.zip]
```

The download/scratch cache lives in gitignored `.provision-<name>/`.

## Verifying without rebuilding

The byte-exact contract is the manifest hash, not the archive bytes, so you can
check any candidate toolchain you already have without re-running the installer:

```sh
oracle provision <name> verify <tree-dir>
```

or, equivalently, with coreutils from the tree's base directory:

```sh
cd <tree-dir> && sha256sum -c <repo>/oracles/<c>/<NAME>.sha256
```

The per-compiler [`oracles/bcc/BC2.md`](../oracles/bcc/BC2.md) and
[`oracles/msc/MSC500.md`](../oracles/msc/MSC500.md) documents cover manual
acquisition and the expected extraction layout in detail.

## How long it takes

- **BCC** — ~10s (pure host-side archive extraction).
- **MSC** — ~3 min, dominated by the 12 sequential DOSBox-X launches that build
  the combined libraries with the real `LIB.EXE`.

## Why not just run the real installer?

For fidelity, the ideal would be to run the vendor installer (Borland's
`INSTALL.EXE`, Microsoft's `SETUP.EXE`) directly. Both read the keyboard through
the BIOS and can't be driven headlessly (no batch/response mode reaches the
interactive prompts), and Borland's media additionally uses a proprietary
multi-volume archive its own `UNZIP` can't read. So the recipe reproduces what
the installer *does* rather than running it — but note the bytes are identical
either way: extraction output doesn't depend on which unzip runs, and MSC's only
non-copy step (the combined libraries) is still built by the **vendor `LIB.EXE`**
under DOSBox-X. Every file is gated against the manifest, so "faithful" is
enforced, not assumed.

## Adding another compiler

The provisioner is recipe-driven — no Rust changes are needed for a new
toolchain whose installer fits the existing step types:

1. Capture its `oracles/<c>/<NAME>.toml` descriptor (download URL +
   `archive_sha256`) and `oracles/<c>/<NAME>.sha256` manifest.
2. Write the `[install]` recipe. Available step types:
   - archive-based media: `extract` (unzip), `span` (join split volumes),
     `relocate` (move files within the tree);
   - file-based media: `copy` (verbatim from a named disk image), `lib_build`
     (run a DOS library tool under DOSBox-X), `write` (installer-generated
     files).
3. `oracle provision <name>` and iterate until all files verify.
