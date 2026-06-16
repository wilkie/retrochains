//! Codegen idiom recognition for BCC 2.0 **and** MSC — the first step toward
//! reading a compiled binary back as the constructs its compiler emits, and
//! toward telling the two compilers apart from their code alone.
//!
//! [`recognize`] linearly decomposes a code byte slice (a `_TEXT` segment) into
//! the instruction idioms the backends produce; [`classify`] weighs the
//! compiler-distinctive ones into a verdict. This is the *decoder* counterpart
//! to the byte-exact compilers: every idiom here is a sequence one of them emits
//! for a known C construct (see `specs/bcc/ASM_OUTPUT.md`, `specs/msc/`, and
//! `specs/FINGERPRINTS.md`), grounded in real BCC/MSC output.
//!
//! It is a prototype: a curated, high-confidence idiom set, not a full
//! disassembler. Unrecognized bytes are reported as gaps, so coverage measures
//! how much of the code the catalog explains. For a *robust* compiler verdict,
//! combine this code-level evidence with the symbol/structure fingerprints the
//! rest of the crate already extracts (the `__chkstk`/`__acrtused` externs and
//! `SLIBCE` default-library COMENT that mark MSC, the `TC86` translator COMENT
//! that marks BCC).

use std::fmt::Write as _;

/// Which compiler an idiom points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compiler {
    Bcc,
    Msc,
    /// Structural / generic — emitted by both (and most era compilers).
    Shared,
}

/// How distinctive an idiom is of its compiler (mirrors `specs/FINGERPRINTS.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strength {
    /// A near-conclusive marker for the compiler on its own.
    Strong,
    /// Typical but shared with the era; useful only in aggregate.
    Weak,
}

/// A recognized codegen idiom — an instruction sequence and its meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idiom {
    /// `55 8b ec` — `push bp; mov bp,sp` (function entry). Shared.
    Prologue,
    /// `55 8b ec 83 ec N` — BCC entry plus `sub sp,N` reserving locals.
    PrologueLocals,
    /// `55 8b ec b8 .. .. e8 .. ..` — MSC's framed prologue: entry, load the
    /// locals size, then `call __chkstk`. MSC calls chkstk in every prologue.
    MscChkstkPrologue,
    /// `33 c0 e8 .. ..` — MSC's frameless prologue: `xor ax,ax` (chkstk size 0)
    /// then `call __chkstk`, with no `bp` frame.
    MscChkstkFrameless,
    /// `8b e5 5d c3` — `mov sp,bp; pop bp; ret` (restore-sp near epilogue).
    EpilogueRestoreSp,
    /// `5d c3` — `pop bp; ret` (near epilogue).
    EpilogueNear,
    /// `5d cb` — `pop bp; retf` (far epilogue, compact/large/huge models).
    EpilogueFar,
    /// `eb 00` — `jmp $+2`: BCC emits a jump to the exit even when the epilogue
    /// is the next instruction. Distinctive of BCC; MSC doesn't.
    BccExitJump,
    /// `33 c0` — `xor ax,ax`: BCC's return-0 / zero.
    BccZeroAx,
    /// `2b c0` — `sub ax,ax`: MSC's return-0. BCC always uses `xor` (`33 c0`),
    /// so this discriminates the two.
    MscZeroAx,
    /// `b8 lo hi` — `mov ax,imm16` (load a literal or relocated address).
    LoadImmAx,
    /// `50` — `push ax` (push a cdecl argument).
    PushAx,
    /// `e8 lo hi` — `call near` (relative).
    NearCall,
    /// `9a o o s s` — `call far`.
    FarCall,
    /// `59` — `pop cx`: BCC's one-argument cdecl cleanup after a call.
    CdeclPop1,
    /// `83 c4 N` — `add sp,N`: discard N bytes of cdecl arguments (MSC's style).
    CdeclPopN,
}

impl Idiom {
    /// Which compiler this idiom points at.
    #[must_use]
    pub fn compiler(self) -> Compiler {
        match self {
            Idiom::PrologueLocals | Idiom::BccExitJump | Idiom::BccZeroAx | Idiom::CdeclPop1 => {
                Compiler::Bcc
            }
            Idiom::MscChkstkPrologue | Idiom::MscChkstkFrameless | Idiom::MscZeroAx => Compiler::Msc,
            _ => Compiler::Shared,
        }
    }

    /// How strongly this idiom points at its compiler.
    #[must_use]
    pub fn strength(self) -> Strength {
        match self {
            // Near-conclusive: the redundant exit jump (BCC) and the chkstk
            // prologue / sub-based zero (MSC) don't appear in the other.
            Idiom::BccExitJump
            | Idiom::MscChkstkPrologue
            | Idiom::MscChkstkFrameless
            | Idiom::MscZeroAx => Strength::Strong,
            _ => Strength::Weak,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Idiom::Prologue => "prologue (push bp; mov bp,sp)",
            Idiom::PrologueLocals => "BCC prologue + sub sp (reserve locals)",
            Idiom::MscChkstkPrologue => "MSC prologue + call __chkstk",
            Idiom::MscChkstkFrameless => "MSC frameless prologue + call __chkstk",
            Idiom::EpilogueRestoreSp => "epilogue (mov sp,bp; pop bp; ret)",
            Idiom::EpilogueNear => "epilogue (pop bp; ret)",
            Idiom::EpilogueFar => "epilogue (pop bp; retf)",
            Idiom::BccExitJump => "BCC exit jump (jmp $+2)",
            Idiom::BccZeroAx => "BCC return 0 (xor ax,ax)",
            Idiom::MscZeroAx => "MSC return 0 (sub ax,ax)",
            Idiom::LoadImmAx => "load ax,imm16",
            Idiom::PushAx => "push ax (arg)",
            Idiom::NearCall => "call near",
            Idiom::FarCall => "call far",
            Idiom::CdeclPop1 => "cdecl cleanup (pop cx)",
            Idiom::CdeclPopN => "cdecl cleanup (add sp,N)",
        }
    }
}

/// One byte of an idiom pattern: a fixed value or an operand wildcard.
#[derive(Clone, Copy)]
enum Bm {
    Lit(u8),
    Any,
}

struct Def {
    idiom: Idiom,
    pat: &'static [Bm],
}

use Bm::{Any as A, Lit as L};

/// The idiom catalog, ordered most-specific-first so a longer idiom wins over a
/// prefix of it at the same offset (e.g. `MscChkstkPrologue` and
/// `PrologueLocals` before `Prologue`; `MscChkstkFrameless` before `BccZeroAx`).
/// MSC's chkstk prologue is its `b8.. e8..` — the call follows the size load
/// *immediately*, where BCC would push an argument (`50`) in between.
const IDIOMS: &[Def] = &[
    Def { idiom: Idiom::MscChkstkPrologue, pat: &[L(0x55), L(0x8b), L(0xec), L(0xb8), A, A, L(0xe8), A, A] },
    Def { idiom: Idiom::PrologueLocals, pat: &[L(0x55), L(0x8b), L(0xec), L(0x83), L(0xec), A] },
    Def { idiom: Idiom::Prologue, pat: &[L(0x55), L(0x8b), L(0xec)] },
    Def { idiom: Idiom::EpilogueRestoreSp, pat: &[L(0x8b), L(0xe5), L(0x5d), L(0xc3)] },
    Def { idiom: Idiom::EpilogueNear, pat: &[L(0x5d), L(0xc3)] },
    Def { idiom: Idiom::EpilogueFar, pat: &[L(0x5d), L(0xcb)] },
    Def { idiom: Idiom::BccExitJump, pat: &[L(0xeb), L(0x00)] },
    Def { idiom: Idiom::MscChkstkFrameless, pat: &[L(0x33), L(0xc0), L(0xe8), A, A] },
    Def { idiom: Idiom::BccZeroAx, pat: &[L(0x33), L(0xc0)] },
    Def { idiom: Idiom::MscZeroAx, pat: &[L(0x2b), L(0xc0)] },
    Def { idiom: Idiom::FarCall, pat: &[L(0x9a), A, A, A, A] },
    Def { idiom: Idiom::NearCall, pat: &[L(0xe8), A, A] },
    Def { idiom: Idiom::LoadImmAx, pat: &[L(0xb8), A, A] },
    Def { idiom: Idiom::CdeclPopN, pat: &[L(0x83), L(0xc4), A] },
    Def { idiom: Idiom::PushAx, pat: &[L(0x50)] },
    Def { idiom: Idiom::CdeclPop1, pat: &[L(0x59)] },
];

fn matches_at(code: &[u8], at: usize, pat: &[Bm]) -> bool {
    code.len() - at >= pat.len()
        && pat.iter().enumerate().all(|(k, m)| match m {
            Bm::Lit(b) => code[at + k] == *b,
            Bm::Any => true,
        })
}

/// One recognized idiom at a byte offset within the scanned code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdiomMatch {
    pub offset: usize,
    pub len: usize,
    pub idiom: Idiom,
}

/// Linearly decompose `code` (a `_TEXT` segment) into recognized idioms. Scans
/// left to right, consuming the longest matching idiom at each position and
/// skipping one byte where nothing matches (reported as coverage gaps).
#[must_use]
pub fn recognize(code: &[u8]) -> Vec<IdiomMatch> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < code.len() {
        if let Some(def) = IDIOMS.iter().find(|d| matches_at(code, i, d.pat)) {
            out.push(IdiomMatch { offset: i, len: def.pat.len(), idiom: def.idiom });
            i += def.pat.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Which compiler the code idioms point at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Bcc,
    Msc,
    /// Distinctive idioms for both compilers appeared — inconclusive on code
    /// alone (e.g. mixed objects, or a false positive); consult symbol evidence.
    Ambiguous,
    /// No distinctive idiom for either compiler was found.
    Unknown,
}

/// The result of [`classify`]: a verdict plus the distinctive-idiom tallies it
/// rests on and the full idiom decomposition.
#[derive(Debug, Clone)]
pub struct Classification {
    pub verdict: Verdict,
    /// Count of BCC-distinctive (`Strong`) idiom hits.
    pub bcc_evidence: usize,
    /// Count of MSC-distinctive (`Strong`) idiom hits.
    pub msc_evidence: usize,
    pub matches: Vec<IdiomMatch>,
}

/// Decide, from code idioms alone, whether `code` was produced by BCC or MSC.
#[must_use]
pub fn classify(code: &[u8]) -> Classification {
    let matches = recognize(code);
    let strong = |c: Compiler| {
        matches
            .iter()
            .filter(|m| m.idiom.strength() == Strength::Strong && m.idiom.compiler() == c)
            .count()
    };
    let bcc_evidence = strong(Compiler::Bcc);
    let msc_evidence = strong(Compiler::Msc);
    let verdict = match (bcc_evidence > 0, msc_evidence > 0) {
        (true, false) => Verdict::Bcc,
        (false, true) => Verdict::Msc,
        (true, true) => Verdict::Ambiguous,
        (false, false) => Verdict::Unknown,
    };
    Classification { verdict, bcc_evidence, msc_evidence, matches }
}

/// Fraction of `code` bytes the idiom catalog explained (0.0–1.0).
#[must_use]
#[allow(clippy::cast_precision_loss)] // byte counts are exact in f32
pub fn coverage(code: &[u8]) -> f32 {
    if code.is_empty() {
        return 0.0;
    }
    let matched: usize = recognize(code).iter().map(|m| m.len).sum();
    matched as f32 / code.len() as f32
}

/// A human-readable lift of `code`: one line per recognized idiom (with operand
/// values where they're part of the idiom), and `??` lines for gap bytes.
#[must_use]
pub fn summarize(code: &[u8]) -> String {
    let mut out = String::new();
    let mut covered = 0usize;
    for m in recognize(code) {
        while covered < m.offset {
            let _ = writeln!(out, "  {covered:#06x}: ?? {:02x}", code[covered]);
            covered += 1;
        }
        let bytes = &code[m.offset..m.offset + m.len];
        let detail = operand_detail(m.idiom, bytes);
        let _ = writeln!(out, "  {:#06x}: {:<14} {}{detail}", m.offset, hex(bytes), m.idiom.label());
        covered = m.offset + m.len;
    }
    while covered < code.len() {
        let _ = writeln!(out, "  {covered:#06x}: ?? {:02x}", code[covered]);
        covered += 1;
    }
    out
}

fn operand_detail(idiom: Idiom, bytes: &[u8]) -> String {
    match idiom {
        Idiom::LoadImmAx if bytes.len() == 3 => {
            format!("  → ax = {:#06x}", u16::from(bytes[1]) | (u16::from(bytes[2]) << 8))
        }
        Idiom::MscChkstkPrologue if bytes.len() == 9 => {
            format!("  → {} bytes of locals", u16::from(bytes[4]) | (u16::from(bytes[5]) << 8))
        }
        Idiom::CdeclPopN if bytes.len() == 3 => format!("  → sp += {}", bytes[2]),
        Idiom::PrologueLocals if bytes.len() == 6 => format!("  → {} bytes of locals", bytes[5]),
        _ => String::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Read an OMF index (1 byte, or 2 when the high bit of the first is set).
fn take_index(p: &[u8], i: &mut usize) -> u16 {
    let v = u16::from(p.get(*i).copied().unwrap_or(0));
    if v & 0x80 != 0 {
        let lo = u16::from(p.get(*i + 1).copied().unwrap_or(0));
        *i += 2;
        ((v & 0x7f) << 8) | lo
    } else {
        *i += 1;
        v
    }
}

/// Extract the first CODE-class segment's bytes (the `_TEXT`) from an OMF object
/// module, for idiom analysis — unlike the first LEDATA, this skips the CONST /
/// `_DATA` records MSC emits before `_TEXT`. Empty if there is no CODE segment.
#[must_use]
pub fn code_of_obj(obj: &[u8]) -> Vec<u8> {
    let mut reader = obj::ObjReader::new(obj);
    let mut lnames: Vec<String> = vec![String::new()];
    let mut seg_is_code: Vec<bool> = vec![false]; // 1-based, index 0 unused
    let mut code: Vec<u8> = Vec::new();
    while let Ok(Some(rec)) = reader.next() {
        match rec.ty {
            obj::LNAMES => {
                let p = rec.payload;
                let mut i = 0;
                while i < p.len() {
                    let len = usize::from(p[i]);
                    let end = (i + 1 + len).min(p.len());
                    lnames.push(String::from_utf8_lossy(&p[i + 1..end]).into_owned());
                    i = end;
                }
            }
            obj::SEGDEF_16 => {
                let p = rec.payload;
                let mut i = 1; // skip ACBP
                if p.first().is_some_and(|a| (a >> 5) == 0) {
                    i += 3; // absolute: frame + offset
                }
                i += 2; // length
                let _name = take_index(p, &mut i);
                let class = take_index(p, &mut i);
                let is_code = lnames.get(usize::from(class)).is_some_and(|c| c == "CODE");
                seg_is_code.push(is_code);
            }
            obj::LEDATA_16 => {
                let p = rec.payload;
                if p.len() >= 3 && seg_is_code.get(usize::from(p[0])).copied().unwrap_or(false) {
                    let off = usize::from(p[1]) | (usize::from(p[2]) << 8);
                    let data = &p[3..];
                    if off + data.len() > code.len() {
                        code.resize(off + data.len(), 0);
                    }
                    code[off..off + data.len()].copy_from_slice(data);
                }
            }
            _ => {}
        }
    }
    code
}

#[cfg(test)]
mod tests {
    use super::Idiom::*;
    use super::*;

    fn kinds(code: &[u8]) -> Vec<Idiom> {
        recognize(code).into_iter().map(|m| m.idiom).collect()
    }

    // --- BCC samples (real bytes from the tracked BCC objects) ---

    /// `int main(){return 0;}` (small model, MAIN.OBJ): entry, BCC return 0,
    /// the redundant exit jump, near epilogue — and classified as BCC.
    #[test]
    fn bcc_return_zero() {
        let code = [0x55, 0x8b, 0xec, 0x33, 0xc0, 0xeb, 0x00, 0x5d, 0xc3];
        assert_eq!(kinds(&code), [Prologue, BccZeroAx, BccExitJump, EpilogueNear]);
        let c = classify(&code);
        assert_eq!(c.verdict, Verdict::Bcc);
        assert_eq!(coverage(&code), 1.0);
    }

    /// `printf("...")` (HELLO.OBJ): the cdecl call shape — load, **push**, call,
    /// pop-cx cleanup. The push between load and call keeps it from looking like
    /// MSC's chkstk prologue; the exit jump classifies it BCC.
    #[test]
    fn bcc_printf_call() {
        let code = [
            0x55, 0x8b, 0xec, 0xb8, 0x00, 0x00, 0x50, 0xe8, 0x00, 0x00, 0x59, 0x33, 0xc0, 0xeb, 0x00,
            0x5d, 0xc3,
        ];
        assert_eq!(
            kinds(&code),
            [Prologue, LoadImmAx, PushAx, NearCall, CdeclPop1, BccZeroAx, BccExitJump, EpilogueNear],
        );
        assert_eq!(classify(&code).verdict, Verdict::Bcc);
    }

    /// Large model (MAIN_L.OBJ): same shape, far epilogue (`retf`).
    #[test]
    fn bcc_far_model() {
        let code = [0x55, 0x8b, 0xec, 0x33, 0xc0, 0xeb, 0x00, 0x5d, 0xcb];
        assert_eq!(kinds(&code), [Prologue, BccZeroAx, BccExitJump, EpilogueFar]);
        assert_eq!(classify(&code).verdict, Verdict::Bcc);
    }

    // --- MSC samples (real bytes from our byte-exact MSC compiler) ---

    /// `int z(void){return 0;}` (MZ.OBJ): the frameless chkstk prologue then
    /// MSC's `sub ax,ax` zero — both MSC-distinctive.
    #[test]
    fn msc_return_zero() {
        let code = [0x33, 0xc0, 0xe8, 0x00, 0x00, 0x2b, 0xc0, 0xc3];
        assert_eq!(kinds(&code), [MscChkstkFrameless, MscZeroAx]); // trailing c3 is a gap
        let c = classify(&code);
        assert_eq!(c.verdict, Verdict::Msc);
        assert_eq!(c.msc_evidence, 2);
    }

    /// `int l(int a){int b; b=a+1; return b;}` (ML.OBJ): the framed chkstk
    /// prologue (`b8 size; call __chkstk`) classifies it MSC.
    #[test]
    fn msc_framed_chkstk() {
        let code = [
            0x55, 0x8b, 0xec, 0xb8, 0x02, 0x00, 0xe8, 0x00, 0x00, 0x8b, 0x46, 0x04, 0x40, 0x89, 0x46,
            0xfe, 0x8b, 0xe5, 0x5d, 0xc3,
        ];
        assert_eq!(kinds(&code)[0], MscChkstkPrologue);
        assert_eq!(kinds(&code).last(), Some(&EpilogueRestoreSp));
        assert_eq!(classify(&code).verdict, Verdict::Msc);
    }

    /// `int c(void){return g(7);}` (MC.OBJ): frameless chkstk, then a cdecl call
    /// cleaned with `add sp,2` (MSC's style, vs BCC's `pop cx`).
    #[test]
    fn msc_cdecl_call() {
        let code = [
            0x33, 0xc0, 0xe8, 0x00, 0x00, 0xb8, 0x07, 0x00, 0x50, 0xe8, 0x00, 0x00, 0x83, 0xc4, 0x02,
            0xc3,
        ];
        assert_eq!(kinds(&code), [MscChkstkFrameless, LoadImmAx, PushAx, NearCall, CdeclPopN]);
        assert_eq!(classify(&code).verdict, Verdict::Msc);
    }
}
