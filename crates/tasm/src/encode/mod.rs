//! Encode parsed instructions into machine bytes. Each segment is
//! encoded independently after a module-wide pre-pass has resolved
//! every label to `(segment-index, offset-within-segment)`.

use std::collections::{HashMap, HashSet};

use crate::ir::{
    AsmError, AsmResult, FixupKind, FixupReq, Instr, Module, SegItem, Segment,
};


mod emit_instr;
mod helpers;
mod segment;
mod size;

pub(crate) use emit_instr::*;
pub(crate) use helpers::*;
pub(crate) use segment::*;
pub(crate) use size::*;
/// Sites where a `Jcc` (originally emitted as the 2-byte short form)
/// must be widened to the 5-byte inverted-Jcc + near-jmp pattern
/// because the short displacement is out of i8 range. Keyed by
/// `(segment-index, item-index-within-segment.items)`.
type ExpandedJccs = HashSet<(usize, usize)>;

/// One segment's encoded output. The notional `size` (used for the
/// SEGDEF length field) can exceed `bytes.len()` when the segment
/// contains uninitialized padding (`db N dup (?)` items).
pub struct EncodedSeg {
    pub size: u32,
    pub bytes: Vec<u8>,
    pub fixups: Vec<FixupReq>,
}

/// Symbol table built across the entire module before encoding any
/// segment. Maps each defined label/anchor to the segment it lives
/// in (0-based index into `module.segments`) and its offset within
/// that segment.
pub type Symbols = HashMap<String, SymbolLoc>;

#[derive(Debug, Clone, Copy)]
pub struct SymbolLoc {
    pub segment: usize,
    pub offset: u16,
}

pub struct EncodedModule {
    pub segments: Vec<EncodedSeg>,
    pub symbols: Symbols,
}

pub fn encode_module(module: &Module) -> AsmResult<EncodedModule> {
    // Iterative Jcc relaxation: a `JmpCondShort` whose target is
    // beyond ±127 bytes can't fit in the 2-byte short form. Mark
    // those sites for expansion to the 5-byte inverted-Jcc + near-
    // jmp pattern; expanding shifts later code so labels need to
    // be rebuilt, and that may push other Jccs out of range. Loop
    // until the set is stable. The set only grows, so this
    // terminates in at most `#JmpCondShort` iterations and in
    // practice converges in 1–2. Fixture 2627 (`if (x == 0) {
    // 32×x=x+1; }` — the `jne else` is 164 bytes ahead).
    let mut expanded: ExpandedJccs = HashSet::new();
    loop {
        let symbols = build_symbols(module, &expanded)?;
        let mut changed = false;
        for (seg_idx, seg) in module.segments.iter().enumerate() {
            let mut pc: u32 = 0;
            for (item_idx, item) in seg.items.iter().enumerate() {
                match item {
                    SegItem::Label(_) | SegItem::Proc(_) | SegItem::EndProc => {}
                    SegItem::Db(b) => pc += b.len() as u32,
                    SegItem::DwSym(_) | SegItem::DwGroupSym { .. } => pc += 2,
                    SegItem::DdGroupSym { .. } | SegItem::DdSym { .. } => pc += 4,
                    SegItem::Pad(n) => pc += *n,
                    SegItem::Instr(instr) => {
                        if let Instr::JmpCondShort { target, .. } = instr {
                            let already_expanded =
                                expanded.contains(&(seg_idx, item_idx));
                            if !already_expanded
                                && let Some(loc) = symbols.get(target)
                                && loc.segment == seg_idx
                            {
                                let here = pc as i32 + 2;
                                let disp = i32::from(loc.offset) - here;
                                if i8::try_from(disp).is_err() {
                                    expanded.insert((seg_idx, item_idx));
                                    changed = true;
                                }
                            }
                            pc += if already_expanded
                                || expanded.contains(&(seg_idx, item_idx))
                            {
                                5
                            } else {
                                2
                            };
                        } else {
                            pc += instr_size(instr) as u32;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    let symbols = build_symbols(module, &expanded)?;
    let group_idx = build_group_idx(module);
    let segment_idx = build_segment_idx(module);
    let extern_idx = build_extern_idx(module);

    let mut segments = Vec::with_capacity(module.segments.len());
    for (i, seg) in module.segments.iter().enumerate() {
        segments.push(encode_segment(
            i,
            seg,
            &symbols,
            &group_idx,
            &segment_idx,
            &extern_idx,
            &expanded,
        )?);
    }
    let _ = (segment_idx, group_idx, extern_idx); // consumed by encode_segment
    Ok(EncodedModule { segments, symbols })
}

fn build_extern_idx(module: &Module) -> HashMap<String, u8> {
    module
        .externs
        .iter()
        .enumerate()
        .map(|(i, n)| (n.clone(), u8::try_from(i + 1).expect("extdef idx fits")))
        .collect()
}

fn build_symbols(module: &Module, expanded: &ExpandedJccs) -> AsmResult<Symbols> {
    let mut out: Symbols = HashMap::new();
    for (seg_idx, seg) in module.segments.iter().enumerate() {
        let mut pc: u32 = 0;
        for (item_idx, item) in seg.items.iter().enumerate() {
            match item {
                SegItem::Label(name) => {
                    let offset = u16::try_from(pc).map_err(|_| {
                        AsmError::new(0, format!("segment exceeds 64K at label `{name}`"))
                    })?;
                    out.insert(
                        name.clone(),
                        SymbolLoc {
                            segment: seg_idx,
                            offset,
                        },
                    );
                }
                SegItem::Proc(_) | SegItem::EndProc => {}
                SegItem::Db(b) => pc += b.len() as u32,
                SegItem::DwSym(_) | SegItem::DwGroupSym { .. } => pc += 2,
                SegItem::DdGroupSym { .. } | SegItem::DdSym { .. } => pc += 4,
                SegItem::Pad(n) => pc += *n,
                SegItem::Instr(instr) => {
                    let sz = if matches!(instr, Instr::JmpCondShort { .. })
                        && expanded.contains(&(seg_idx, item_idx))
                    {
                        5
                    } else {
                        instr_size(instr)
                    };
                    pc += sz as u32;
                }
            }
        }
    }
    Ok(out)
}

fn build_group_idx(module: &Module) -> HashMap<String, u8> {
    module
        .groups
        .iter()
        .enumerate()
        .map(|(i, g)| (g.name.clone(), u8::try_from(i + 1).expect("group idx fits")))
        .collect()
}

fn build_segment_idx(module: &Module) -> HashMap<String, u8> {
    module
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.clone(), u8::try_from(i + 1).expect("seg idx fits")))
        .collect()
}












#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Reg16, SegAlign, SegCombine};

    fn make_seg(name: &str) -> Segment {
        Segment {
            name: name.into(),
            align: SegAlign::Byte,
            combine: SegCombine::Public,
            class: "CODE".into(),
            items: Vec::new(),
        }
    }

    #[test]
    fn fixture_002_main_body() {
        let mut seg = make_seg("_TEXT");
        seg.items = vec![
            SegItem::Instr(Instr::PushReg16 { reg: Reg16::Bp }),
            SegItem::Instr(Instr::MovReg16Reg16 {
                dst: Reg16::Bp,
                src: Reg16::Sp,
            }),
            SegItem::Instr(Instr::XorReg16Reg16 {
                dst: Reg16::Ax,
                src: Reg16::Ax,
            }),
            SegItem::Instr(Instr::JmpShort("@1@50".into())),
            SegItem::Label("@1@50".into()),
            SegItem::Instr(Instr::PopReg16 { reg: Reg16::Bp }),
            SegItem::Instr(Instr::Ret),
        ];
        let module = Module {
            segments: vec![seg],
            ..Module::default()
        };
        let em = encode_module(&module).unwrap();
        assert_eq!(em.segments[0].bytes, vec![0x55, 0x8B, 0xEC, 0x33, 0xC0, 0xEB, 0x00, 0x5D, 0xC3]);
        assert_eq!(em.segments[0].size, 9);
        assert!(em.segments[0].fixups.is_empty());
    }
}
