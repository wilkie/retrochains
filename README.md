# Retrochains

A clean-room implementation of several old C/C++ compilers written in Rust with
WebAssembly builds for browser-based TypeScript and JavaScript use.

Currently, this is targetting mostly x86 compiler toolchains from the 1980s and
1990s.

## Compilers

* Borland C++ 2.0 compiler toolchain
* Microsoft C++ 5.0 compiler toolchain

## Reproducibility

The real compilers themselves are not made available in this repository. Instead,
there are `sha256` files which show the files used to generate the fixtures. Many
of the compilers used are installed via the floppy disk images found on the
WinWorld archive and should be possible to generate the equivalent compiler
toolchains used here and the file structure expected by the fixture and oracle
harness.

This regeneration is automated. Given only the tracked descriptor and manifest,
`oracle provision <bcc|msc>` downloads the public install media, reassembles the
toolchain, and verifies every file byte-for-byte against the recorded hashes:

```sh
cargo build --workspace --bins
target/debug/oracle provision bcc   # -> ./BC2.zip   (99 files verified)
target/debug/oracle provision msc   # -> ./MSC500.zip (136 files verified)
```

That rebuilds the **compiler toolchains** (binaries, headers, runtime libraries,
and the toolchains' own startup object files), each verified against
`oracles/<c>/<NAME>.sha256`. See **[specs/PROVISIONING.md](specs/PROVISIONING.md)**
for prerequisites, how the pipeline works, verifying an existing toolchain, and
adding another compiler.

### Fixture goldens

The compiler's *per-fixture outputs* — the `.OBJ`/`.ASM`/`.EXE`/`.MAP` each
fixture produces — are a second hash-pinned cache, also gitignored. They are
**not** part of the toolchain archives above; they're regenerated on demand by
driving the provisioned compiler with `xfix`:

```sh
xfix materialize <fixture>           # re-emit a fixture's golden bytes (asserts they match recorded hashes)
xfix verify-all --toolchain ours     # check our reimplementation against every recorded hash (no oracle needed)
```

`verify-all` gates purely on the recorded hashes, so it works on a fresh checkout
with no archives at all; you only need to `provision` (and then `materialize`)
when you want to re-drive the **original** compiler — to add fixtures or inspect a
byte-level diff. See **[specs/FIXTURES.md](specs/FIXTURES.md)** for the fixture
corpus and the capture/verify harness.
