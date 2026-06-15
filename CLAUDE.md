# retrochains — contributor conventions

## File size

Keep source files within a **soft target of 2,000–3,000 lines**. Treat
**4,000 lines as a ceiling**: a file that reaches it should be split into
focused submodules before adding more.

Guidance:
- A `mod.rs` should act as a **facade** — declare submodules, hold shared
  types and the public entry points, and orchestrate the passes. Bulk logic
  belongs in concern-specific sibling modules, not in `mod.rs`.
- Split by concern, not by arbitrary line count. Prefer semantic module names
  (`statements`, `conditions`, `expressions`, …). When one concern genuinely
  exceeds the ceiling, numbered parts (`emitter_assign_1`, `emitter_assign_2`)
  are acceptable.
- When splitting a large `impl` across sibling modules, note that method
  privacy follows the **module holding the `impl` block**, not the module
  where the struct is defined. So methods that call each other across the
  split must be at least `pub(crate)` (a bare `fn` will fail to compile with
  `E0624`). A child module's `use super::*;` does pull in the parent's private
  free items and types, and child modules can read an ancestor struct's
  private fields — so only the *methods* need a visibility bump.

This is a soft convention, enforced by review rather than lint. `locals.rs`
(~4k) is a known borderline case left intact because it is cohesive.

## Byte-exact invariant

This project is a byte-exact reimplementation. The verification harness is the
`xfix` binary (crate `fixtures`), not `cargo test`:

```
cargo build --workspace --bins
target/debug/xfix verify-all --toolchain ours
```

The current baseline is **4129 pass, 0 fail** out of 4129 BCC fixtures — the
BCC pool is fully green. Any refactor — especially pure code moves — must
reproduce this result exactly (all 4129 passing). `verify-all` exits non-zero
whenever any fixture fails, so check the printed summary, not just the exit
code.

The MSC toolchain has its own pool — `verify-all --toolchain ours --compiler
msc` — currently **4004 pass / 0 fail** out of 4004 — also fully green. (A
fixture can be MSC-only when our BCC can't yet match real BCC; capture both
with `xfix capture --compiler {bcc,msc}` and add both
`invocation.{bcc,msc}.toml`.)

## Goldens are a hash-pinned, reproducible cache

Compiler binary outputs (`.OBJ`, `.ASM`, `.EXE`, `.MAP`) plus captured
`stdout`/`stderr` are **not** tracked in git. The byte-exact contract is the
`sha256` recorded for each output in the tracked, release-keyed manifest
`fixtures/<path>/expected/<family>/<RELEASE>.toml` (e.g. `expected/bcc/BC2.toml`),
and `verify-all` gates on that hash — it passes/fails identically whether or not
the golden bytes are on disk. The bytes themselves live under `artifacts/`,
mirroring the fixtures tree and keyed by family/release
(`artifacts/<path>/<family>/<RELEASE>/`), gitignored so git prunes the whole
cache in one step (see `artifacts/README.md`). Regenerate them locally with
`xfix materialize <fixture>` (re-drives the oracle, refuses to write unless the
reproduction matches the recorded hash). So on a fresh checkout, `verify-all`
works immediately, and you only `materialize` a fixture when you want its
byte-level diff for debugging.
They were purged from history once (see git log) — never re-track them; the
`.gitignore` keeps captures from re-adding them.
