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

See **[specs/PROVISIONING.md](specs/PROVISIONING.md)** for prerequisites, how the
pipeline works, verifying an existing toolchain, and adding another compiler.
