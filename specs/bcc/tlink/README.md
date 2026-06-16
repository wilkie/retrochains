# TLINK — the linker

Discoveries about `TLINK.EXE` go here. Suggested files:

- `DRIVER.md` — command-line surface (the unusual comma-separated argument
  form: `tlink objfiles, exefile, mapfile, libfiles, deffile`), response
  files, common flags.
- `OMF_CONSUMPTION.md` — which OMF records TLINK actually pays attention
  to and what it does with each.
- `SEGMENT_LAYOUT.md` — how TLINK orders segments in the output, group
  alignment, padding rules.
- `FIXUPS.md` — fixup resolution: near/far, segment vs offset, self-relative.
- `MZ_OUTPUT.md` — the MZ executable header and image layout TLINK
  produces.
- `LIBRARY_RESOLUTION.md` — how TLINK searches `.LIB` archives, what gets
  pulled in, symbol resolution order.
- `OVERLAYS.md` — overlay support if/when we encounter it.

Always link discoveries back to the fixture that demonstrates them.
