//! Turbo Link 4.0 reimplementation. Consumes OMF object files (and `.LIB`
//! archives) produced by the Borland C++ 2.0 toolchain (BCC/TASM/TLIB) and
//! produces DOS MZ executables, byte-for-byte matching TLINK.EXE.
//!
//! Pipeline: [`omf::parse`] each input object → resolve unresolved externals
//! against the supplied libraries, pulling in defining members → [`link::link`]
//! combines and resolves the modules into a load [`link::Image`] →
//! [`mz::write`] serializes the MZ executable. The standalone-linker fixtures
//! (`fixtures/c/linking/standalone/`) are the byte-exact contract.

pub mod archive;
pub mod link;
pub mod map;
pub mod mz;
pub mod omf;

use std::collections::HashSet;

pub use archive::ArchiveError;
pub use link::LinkError;
pub use omf::{Module, ParseError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("parsing {module}: {source}")]
    Parse { module: String, source: ParseError },
    #[error("reading library {lib}: {source}")]
    Library { lib: String, source: ArchiveError },
    #[error(transparent)]
    Link(#[from] LinkError),
}

/// Public symbols a module defines.
fn defined_in(module: &Module) -> impl Iterator<Item = &str> {
    module.pubdefs.iter().map(|p| p.name.as_str())
}

/// External symbols a module references (skipping the 1-based index-0 slot).
fn needed_in(module: &Module) -> impl Iterator<Item = &str> {
    module.extdefs.iter().skip(1).map(String::as_str)
}

/// Link object modules against zero or more libraries into a DOS MZ executable.
///
/// Objects are linked unconditionally, in command-line order. Each library is
/// searched for members that satisfy a still-unresolved external; a pulled
/// member can introduce new externals, which are resolved in turn (transitive
/// pull), until no library member resolves anything new.
///
/// # Errors
/// Returns [`Error::Parse`]/[`Error::Library`] if an input isn't valid OMF the
/// linker handles, or [`Error::Link`] for an unresolved symbol / unsupported
/// fixup during layout.
pub fn link_objects(
    objects: &[(String, Vec<u8>)],
    libraries: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, Error> {
    Ok(mz::write(&link_image(objects, libraries)?))
}

/// Like [`link_objects`] but returns the full [`link::Image`] — for callers that
/// also want the `.MAP` listing (via [`map::format`]).
///
/// # Errors
/// Same as [`link_objects`].
pub fn link_image(
    objects: &[(String, Vec<u8>)],
    libraries: &[(String, Vec<u8>)],
) -> Result<link::Image, Error> {
    Ok(link::link(&resolved_modules(objects, libraries)?)?)
}

/// Parse the named objects and pull the library members they require, returning
/// the modules in final link order (named objects first, then pulled members in
/// library order). Exposed for tools that inspect the linked module set.
///
/// # Errors
/// Same as [`link_objects`] (parse/library errors).
pub fn resolved_modules(
    objects: &[(String, Vec<u8>)],
    libraries: &[(String, Vec<u8>)],
) -> Result<Vec<Module>, Error> {
    let mut modules = Vec::with_capacity(objects.len());
    for (name, bytes) in objects {
        let module =
            omf::parse(bytes).map_err(|source| Error::Parse { module: name.clone(), source })?;
        modules.push(module);
    }

    // Parse each library's members up front; pull them in on demand below.
    let mut members: Vec<Option<Module>> = Vec::new();
    for (name, bytes) in libraries {
        let parsed =
            archive::members(bytes).map_err(|source| Error::Library { lib: name.clone(), source })?;
        members.extend(parsed.into_iter().map(Some));
    }

    // Resolve externals: repeatedly pull the first library member (in library
    // order) that defines a currently-unresolved symbol, until a full pass
    // pulls nothing. We record each pulled member's library index so the final
    // placement can follow library order, not pull order.
    let object_count = modules.len();
    let mut pulled_keys: Vec<usize> = Vec::new();
    loop {
        let defined: HashSet<&str> = modules.iter().flat_map(defined_in).collect();
        let unresolved: HashSet<&str> = modules
            .iter()
            .flat_map(needed_in)
            .filter(|s| !defined.contains(s))
            .collect();
        if unresolved.is_empty() {
            break;
        }
        let mut pulled = false;
        for (slot_idx, slot) in members.iter_mut().enumerate() {
            let defines_needed = slot
                .as_ref()
                .is_some_and(|m| defined_in(m).any(|s| unresolved.contains(s)));
            if defines_needed {
                modules.push(slot.take().expect("slot just checked Some"));
                pulled_keys.push(slot_idx);
                pulled = true;
                break;
            }
        }
        if !pulled {
            // Nothing left to satisfy the remaining externals — let the layout
            // pass surface the unresolved symbol with its name.
            break;
        }
    }

    // TLINK lays pulled members down in library order (ascending member index),
    // independent of the order resolution discovered them. Reorder the pulled
    // tail (the named objects keep their command-line order).
    let mut tail: Vec<(usize, Module)> =
        pulled_keys.into_iter().zip(modules.drain(object_count..)).collect();
    tail.sort_by_key(|(key, _)| *key);
    modules.extend(tail.into_iter().map(|(_, module)| module));

    Ok(modules)
}
