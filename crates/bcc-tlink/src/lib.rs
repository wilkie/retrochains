//! Turbo Link 4.0 reimplementation. Consumes OMF object files produced by
//! the Borland C++ 2.0 toolchain (BCC/TASM) and produces DOS MZ executables,
//! byte-for-byte matching TLINK.EXE.
//!
//! Pipeline: [`omf::parse`] each input object → [`link::link`] combines and
//! resolves them into a load [`link::Image`] → [`mz::write`] serializes the
//! MZ executable. The standalone-linker fixtures
//! (`fixtures/c/linking/standalone/`) are the byte-exact contract.

pub mod link;
pub mod mz;
pub mod omf;

pub use link::LinkError;
pub use omf::ParseError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parsing {module}: {source}")]
    Parse { module: String, source: ParseError },
    #[error(transparent)]
    Link(#[from] LinkError),
}

/// Link object modules (given as `(name, bytes)` in command-line order) into
/// a DOS MZ executable.
///
/// # Errors
/// Returns [`Error::Parse`] if any input isn't valid OMF the linker handles,
/// or [`Error::Link`] for an unresolved symbol / unsupported fixup.
pub fn link_objects(objects: &[(String, Vec<u8>)]) -> Result<Vec<u8>, Error> {
    let mut modules = Vec::with_capacity(objects.len());
    for (name, bytes) in objects {
        let module = omf::parse(bytes)
            .map_err(|source| Error::Parse { module: name.clone(), source })?;
        modules.push(module);
    }
    let image = link::link(&modules)?;
    Ok(mz::write(&image))
}
