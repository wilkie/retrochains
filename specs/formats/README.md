# Shared file formats

References for file formats that more than one tool in the Borland chain
touches. Per-tool quirks (what BCC actually emits, what TLINK actually
consumes) live in the respective tool's directory; this directory is the
general spec.

Suggested files (create lazily as we cover each format):

- `OMF.md` — Intel/Microsoft Object Module Format. Record types, record
  layout, checksum computation, name-list / LNAMES, segment / group records,
  COMENT subtypes, debug-info records.
- `MZ_EXE.md` — the DOS MZ executable: header layout, relocation table,
  image layout, entry point.
- `LIB_ARCHIVE.md` — the Microsoft `.LIB` archive format (page-based, with
  a dictionary at the end). What `CS.LIB`, `MATHS.LIB`, etc. actually
  contain.
- `DEBUG_INFO.md` — the Borland-specific debug-info format embedded in OBJ
  and EXE (Turbo Debugger symbols).

Cite the spec source (Intel TIS, Borland docs, observed-from-fixtures) per
section so future readers can re-verify.
