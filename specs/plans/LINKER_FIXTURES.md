# Linker fixtures — plan

Goal: a **wide pool of linked-executable goldens** for free, by reusing the
existing compile corpus, plus a focused area for linker-intrinsic behavior.
Feeds the linker reimplementation (`crates/bcc-tlink`, future `crates/msc-link`)
and EXE-level fingerprinting (`specs/linkers/DIFFERENCES.md`).

## Two buckets

1. **Link as a *stage* on existing complete-program fixtures (broad reuse).**
   ~3,230 of 4,252 C fixtures have `main()` and are linkable. We add a link step
   that produces the `.EXE` alongside the `.OBJ`, in the same fixture, keyed by
   family/release like every other output. No parallel tree; the linker gets
   exercised across the whole feature space.

2. **A dedicated `fixtures/c/linking/` area (linker-intrinsic unit tests).**
   Multi-OBJ combination, library/symbol resolution order, segment/group layout
   edge cases, overlays, `.DEF` exports, `/MAP` output — things single-TU
   compile+link can't reach. Input is OBJ file(s); invocation drives the linker
   standalone (`invocation.tlink.toml`, `tool = "tlink"`). (Built out later.)

## The mechanism: `link_args` (mirrors `asm_args`)

The harness already chains arg-sets in one DOSBox session (`asm_args` runs `-c`
then `-S` to capture both OBJ and ASM via `run_chained`). The link stage is a
**third arg-set**, `link_args`, running the *same family tool* in compile+link
mode (BCC/CL emit the EXE; they invoke TLINK/LINK internally):

```toml
# invocation.bcc.toml
tool = "bcc"
args      = ["-c", "-ms", "HELLO.C"]          # -> HELLO.OBJ  (existing)
asm_args  = ["-S", "-ms", "HELLO.C"]          # -> HELLO.ASM  (existing, optional)
link_args = ["-LC:\\LIB", "-ms", "HELLO.C"]   # -> HELLO.EXE  (new: compile+link)
```
```toml
# invocation.msc.toml
tool = "cl"
args      = ["/c", "/AS", "HELLO.C"]          # -> HELLO.OBJ
link_args = ["/AS", "HELLO.C"]                # -> HELLO.EXE  (CL reads LIB env)
```

The chained run produces OBJ (+ASM) + EXE; the harness already collects every
output file and records each in the release-keyed manifest
(`expected/<family>/<RELEASE>.toml`), with bytes in `artifacts/.../`. **No new
output/verify plumbing** — the EXE is just another gated output.

Implementation footprint (all mirroring `asm_args`):
- `OracleInvocation.link_args` + `with_link_args`; `run()` builds the arg-set
  list `[args, asm_args?, link_args?]` and chains when >1.
- `Invocation.link_args` (serde) in the fixture schema.
- `run_oracle` threads it through.

Why compile+link (not `tlink` directly) for the stage: `run_chained` uses one
tool per session, and BCC/CL *are* the natural driver (they pick the right
startup `C0<model>.OBJ` + library). It recompiles in the link step (byte-
identical, deterministic) — acceptable. Isolating *pure* linker behavior is what
bucket 2 (standalone `tlink` on hand-built OBJs) is for.

## Rolling it out to the pool

A script derives `link_args` from each complete-program fixture's compile args
and adds it to the invocation, per family:
- BCC: drop `-c`, prepend `-LC:\LIB`, keep model + source.
- MSC: drop `/c` and listing flags (`/Fa`…), keep model + source.

Then `xfix capture` (batched, parallel) records the EXE goldens. Fixtures whose
link fails (unresolved refs beyond the C runtime) simply don't gain an EXE — the
roll-out skips them and logs the count.

## Status / order

1. Land the `link_args` mechanism + validate on one fixture. ← do first
2. Roll out across the linkable corpus; batch-capture the EXE goldens (the pool).
3. Build the dedicated `linking/` area for linker-intrinsic cases.
4. Stand up the MZ reader + EXE fingerprinting against the pool.
