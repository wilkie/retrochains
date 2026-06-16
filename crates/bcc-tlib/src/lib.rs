//! Turbo Librarian 2.0 reimplementation. Builds and maintains OMF `.LIB`
//! archives byte-for-byte matching TLIB.EXE. See `specs/formats/LIB_ARCHIVE.md`
//! and `specs/bcc/tlib/`.

pub mod dict;
pub mod write;

pub use write::{WriteError, build_library};
