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
pub mod overlay;

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

/// Link with VROOMM overlays: the modules after `/o` (named in `overlaid`, by
/// object filename) are moved into an appended `FBOV` overlay area behind
/// `INT 3F` stubs, with the disk-overlay manager force-pulled from `OVERLAY.LIB`.
/// Returns the full EXE (resident MZ image + overlay area).
///
/// # Errors
/// Same as [`link_objects`].
pub fn link_overlay(
    objects: &[(String, Vec<u8>)],
    libraries: &[(String, Vec<u8>)],
    overlaid: &HashSet<String>,
    exe_name: &str,
) -> Result<Vec<u8>, Error> {
    let mut modules = resolve(objects, libraries, &[overlay::MANAGER_ROOT])?;

    // Transform each overlaid object's code into a stub; collect the overlays
    // (for the FBOV area) and the per-symbol thunk offsets (for the redirect).
    let mut overlays: Vec<overlay::Overlay> = Vec::new();
    let mut thunks: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    for (i, (name, _)) in objects.iter().enumerate() {
        if overlaid.contains(name) {
            if let Some((ovl, th)) = overlay::make_stub(&mut modules[i]) {
                overlays.push(ovl);
                thunks.extend(th);
            }
        }
    }

    // Reserve _EXEINFO_ and define the linker-generated symbols OVRMAN needs.
    // The table holds one 8-byte entry per resident segment (distinct name/class
    // across all modules), then the exe name and date.
    let seg_count = {
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        for m in &modules {
            for s in m.segdefs.iter().skip(1) {
                seen.insert((s.name.as_str(), s.class.as_str()));
            }
        }
        seen.len()
    };
    let name = exe_name.to_ascii_lowercase();
    let entries_end = seg_count * 8;
    let table_size = entries_end + name.len() + 1 + 7;
    if let Some((mi, si)) = modules.iter().enumerate().find_map(|(mi, m)| {
        m.segdefs.iter().position(|s| s.name == "_EXEINFO_").map(|si| (mi, si))
    }) {
        let seg = &mut modules[mi].segdefs[si];
        seg.length = table_size as u16;
        seg.data = vec![0u8; table_size];
        seg.has_data = true;
        let exedate = entries_end + name.len() + 1 + 3;
        for (nm, off) in [
            ("__SEGTABLE__", 0),
            ("__SEGTABEND__", entries_end),
            ("__EXENAME__", entries_end),
            ("__EXEDATE__", exedate),
        ] {
            modules[mi].pubdefs.push(omf::PubDef {
                name: nm.to_string(),
                base_segment: si as u8,
                offset: off as u16,
                absolute_frame: 0,
            });
        }
    }

    // Segment-name → group, for the table's flags/size.
    let mut seg2grp: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in &modules {
        for g in &m.grpdefs {
            for &s in &g.segments {
                if let Some(seg) = m.segdefs.get(usize::from(s)) {
                    seg2grp.insert(seg.name.clone(), g.name.clone());
                }
            }
        }
    }

    let mut image = link::link(&modules, &thunks)?;

    // Fill _EXEINFO_ with the segment table, using the final layout.
    let resident: Vec<overlay::ResidentSeg> = image
        .map
        .segments
        .iter()
        .map(|s| overlay::ResidentSeg {
            start: s.start,
            len: s.length,
            class: s.class.clone(),
            group: seg2grp.get(&s.name).cloned(),
            overlay_stub: s.class == "STUBSEG" && s.name != "_1STUB_",
        })
        .collect();
    let exeinfo_start = image
        .map
        .segments
        .iter()
        .find(|s| s.name == "_EXEINFO_")
        .map(|s| s.start);
    let date_tail = [0u8, 0, 0x70, 0x17, 0x04, 0xc7, 0x07];
    let table = overlay::segment_table(&resident, exe_name, &date_tail);

    // The resident image (incl. BSS/stack, zero-filled) is fully present in the
    // file so the overlay area can follow it; nothing goes to minalloc.
    image.file_image.resize(image.mem_size, 0);
    if let Some(start) = exeinfo_start {
        image.file_image[start..start + table.len()].copy_from_slice(&table);
        // Each table entry's `para` field is a load-relative segment value; DOS
        // relocates it. Append one relocation per entry (offset i*8 within the
        // _EXEINFO_ frame), after the resident relocations.
        let frame = (start >> 4) as u16;
        for i in 0..seg_count {
            image.relocations.push(((i * 8) as u16, frame));
        }
    }

    let mut exe = mz::write(&image);
    // Append the FBOV overlay area (beyond the MZ-declared image).
    let exeinfo_file_off = mz::HEADER_SIZE + exeinfo_start.unwrap_or(0);
    exe.extend_from_slice(&overlay::fbov_area(&overlays, exeinfo_file_off, seg_count));

    Ok(exe)
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
    Ok(link::link(&resolved_modules(objects, libraries)?, &std::collections::HashMap::new())?)
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
    resolve(objects, libraries, &[])
}

/// Like [`resolved_modules`] but seeds resolution with extra external references
/// that aren't named by any object — used to force-pull the overlay manager
/// from `OVERLAY.LIB` when `/o` is active (TLINK injects these references).
///
/// # Errors
/// Same as [`resolved_modules`].
pub fn resolve(
    objects: &[(String, Vec<u8>)],
    libraries: &[(String, Vec<u8>)],
    forced: &[&str],
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
        let mut unresolved: HashSet<&str> = modules
            .iter()
            .flat_map(needed_in)
            .filter(|s| !defined.contains(s))
            .collect();
        unresolved.extend(forced.iter().copied().filter(|s| !defined.contains(s)));
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
