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

The current baseline is **4083 pass, 2 fail** out of 4085 fixtures. The two
failures (`891-int-global-pointer-subscript-cmp-const-obj` and
`893-int-global-pointer-subscript-as-call-arg-obj`) are pre-existing on
`main`. Any refactor — especially pure code moves — must reproduce this
result exactly (same counts, same two failing fixtures). `verify-all` exits
non-zero whenever any fixture fails, so check the printed summary, not just
the exit code.
