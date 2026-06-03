# BC2.zip — Borland C++ 2.0 oracle distribution

This file documents the contents of `BC2.zip`, the
[Borland C++ 2.0][release] DOS toolchain we use as the *primary* oracle —
the reference whose byte-exact output `crates/bcc` reproduces. The oracle
crate (`crates/oracle/`) lazily unpacks it into `.bc2/` (gitignored) and
drives `BCC.EXE`, `TASM.EXE`, and `TLINK.EXE` under DOSBox.

[release]: https://winworldpc.com/product/borland-cpp/2x

**`BC2.zip` itself is gitignored** because Borland/Embarcadero has never
licensed the binaries for redistribution. The companion
[`BC2.sha256`](BC2.sha256) is the integrity anchor: if a contributor's
Borland C++ 2.0 install hashes to the same values, they have the same
toolchain we used to capture goldens, and their `crates/bcc` runs will
produce byte-identical OBJs. This mirrors the
[`MSC500.sha256`](MSC500.sha256) / [`MSC500.md`](MSC500.md) arrangement
used for the second compiler target.

## How to acquire Borland C++ 2.0

Borland C++ 2.0 (1991-04-23) shipped on floppy disks. It's been treated
as abandonware for ~30 years and is widely available; we have no opinion
on jurisdiction-specific legal exposure.

Practical sources:

- [WinWorldPC](https://winworldpc.com/product/borland-cpp/2x) — curated,
  stable, hosts floppy images and extractable archives. The most popular
  source.
- [archive.org](https://archive.org) — search "Borland C++ 2.0"; multiple
  uploads with varying provenance.

We only need the three binaries plus the headers and libraries listed
below; a full IDE install is not required.

## Extraction layout

`BC2.zip` wraps everything in a top-level `BC2/` directory, so the
manifest paths all begin with `BC2/`:

```
BC2/BIN/      BCC.EXE (compiler driver), TASM.EXE (assembler),
              TLINK.EXE (linker) — the three binaries the oracle runs.
BC2/INCLUDE/  Standard + Borland headers (stdio.h, string.h, dos.h, ...).
BC2/INCLUDE/SYS/ POSIX-style sub-headers (sys/stat.h, sys/types.h, ...).
BC2/LIB/      Runtime/CRT libraries and startup objects across the
              memory-model matrix (C/S/M/C/L variants).
```

### Top-level summary

| Path             | Files | Total bytes |
|------------------|------:|------------:|
| `BC2/BIN/`       |     3 |     681,538 |
| `BC2/INCLUDE/`   |    47 |     365,452 |
| `BC2/INCLUDE/SYS/`|    3 |       2,230 |
| `BC2/LIB/`       |    46 |   1,783,225 |
| **Total**        | **99** | **2,832,445** |

## Verifying a candidate install

```bash
# 1. Extract your BC2.zip somewhere.
unzip -d /tmp/bc2 BC2.zip

# 2. Hash-check against the manifest (paths are BC2/-prefixed, so cd to
#    the parent of the extracted BC2/ directory).
cd /tmp/bc2
sha256sum -c <path-to-repo>/BC2.sha256
```

Every entry should report `OK`. A single mismatch means your copy differs
from ours and goldens captured against it will diverge trivially even if
the compiler behavior is functionally identical.

## Notes

- File timestamps inside the zip are `1991-04-23 01:00` — Borland C++
  2.0's release date. The `2025-01-09` directory entries are filesystem
  metadata from the local extract that produced the zip; they don't
  affect the SHAs of the contained files.
- The oracle lazy-extracts `BC2.zip` into `.bc2/` (gitignored) on first
  use and from then on drives those binaries under DOSBox. See
  [`specs/RUNNING_BCC.md`](specs/RUNNING_BCC.md).
