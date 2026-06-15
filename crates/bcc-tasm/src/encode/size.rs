use super::*;

pub(crate) fn instr_size(instr: &Instr) -> usize {
    match instr {
        Instr::SegOverride { inner, .. } => 1 + instr_size(inner),
        Instr::MovSregBpRel { offset, .. } => 1 + bp_rel_modrm_size(*offset),
        Instr::MovAlAtAddr { .. } => 3,
        Instr::MovByteAtAddrImm8 { .. } => 5,
        Instr::PushImm8Sx { .. } => 2,
        Instr::Enter { .. } => 4,
        Instr::Leave => 1,
        Instr::Ret => 1,
        Instr::RetImm16 { .. } => 3,
        Instr::Retf => 1,
        Instr::RetfImm16 { .. } => 3,
        Instr::PushReg16 { .. }
        | Instr::PopReg16 { .. }
        | Instr::Pushf
        | Instr::IncReg16 { .. }
        | Instr::DecReg16 { .. } => 1,
        Instr::MovReg16Reg16 { .. }
        | Instr::XorReg16Reg16 { .. }
        | Instr::AddReg16Reg16 { .. }
        | Instr::AdcReg16Reg16 { .. }
        | Instr::SubReg16Reg16 { .. }
        | Instr::SbbReg16Reg16 { .. }
        | Instr::AndReg16Reg16 { .. }
        | Instr::OrReg16Reg16 { .. }
        | Instr::CmpReg16Reg16 { .. } => 2,
        Instr::CmpReg16Imm8 { .. } | Instr::CmpAxImm { .. } | Instr::AddAxImm { .. } | Instr::SubAxImm { .. } => 3,
        Instr::CmpReg16Imm16 { .. } => 4,
        Instr::CmpBpRelImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::CmpBpRelImm16 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::JmpShort(_) | Instr::ShlAxCl | Instr::SarAxCl | Instr::ShrAxCl => 2,
        Instr::ShlReg16Cl { .. } | Instr::SarReg16Cl { .. } | Instr::ShrReg16Cl { .. } => 2,
        Instr::ShlReg8Cl { .. } | Instr::SarReg8Cl { .. } | Instr::ShrReg8Cl { .. } => 2,
        Instr::Cwd => 1,
        Instr::JmpCondShort { .. } => 2,
        Instr::JmpIndirectCsTableBx { .. } => 5,
        Instr::JmpIndirectCsBxDisp { .. } => 4,
        Instr::LoopShort { .. } => 2,
        Instr::MovBpRelAx { offset } | Instr::MovBpRelReg16 { offset, .. } => {
            1 + bp_rel_modrm_size(*offset)
        }
        Instr::MovAxFromCsBx => 3,
        Instr::MovAxFromCsBxDisp { .. } => 4,
        Instr::XchgReg8Reg8 { .. } => 2,
        Instr::MovAlBpSiDisp { .. } | Instr::CmpAlBpSiDisp { .. } => 3,
        Instr::CmpBpSiDispImm8 { .. } => 4,
        Instr::MovReg16OffsetSym { .. } => 3,
        Instr::MovReg16GroupSymBxDisp { .. } => 4,
        Instr::AddReg16GroupSymBxDisp { .. } => 4,
        Instr::CmpGroupSymBxDispImm8 { .. } => 5,
        Instr::IncGroupSymBxDisp { .. } | Instr::DecGroupSymBxDisp { .. }
        | Instr::IncGroupSymBxDispByte { .. } | Instr::DecGroupSymBxDispByte { .. }
        | Instr::IncGroupSymSiDispByte { .. } | Instr::DecGroupSymSiDispByte { .. }
        | Instr::IncGroupSymDiDispByte { .. } | Instr::DecGroupSymDiDispByte { .. } => 4,
        Instr::AddGroupSymBxDispImm8Sx { .. } | Instr::SubGroupSymBxDispImm8Sx { .. } => 5,
        Instr::AddGroupSymBxDispImm16 { .. } | Instr::SubGroupSymBxDispImm16 { .. } => 6,
        Instr::AddGroupSymBxDispReg16 { .. } | Instr::SubGroupSymBxDispReg16 { .. }
        | Instr::AddGroupSymBxDispReg8 { .. } | Instr::SubGroupSymBxDispReg8 { .. }
        | Instr::AndGroupSymBxDispReg8 { .. } | Instr::OrGroupSymBxDispReg8 { .. }
        | Instr::XorGroupSymBxDispReg8 { .. }
        | Instr::AddGroupSymSiDispReg8 { .. } | Instr::AddGroupSymDiDispReg8 { .. }
        | Instr::SubGroupSymSiDispReg8 { .. } | Instr::SubGroupSymDiDispReg8 { .. }
        | Instr::AndGroupSymSiDispReg8 { .. } | Instr::AndGroupSymDiDispReg8 { .. }
        | Instr::OrGroupSymSiDispReg8 { .. } | Instr::OrGroupSymDiDispReg8 { .. }
        | Instr::XorGroupSymSiDispReg8 { .. } | Instr::XorGroupSymDiDispReg8 { .. } => 4,
        Instr::CmpGroupSymBxDispImm16 { .. } => 6,
        Instr::CmpByteGroupSymBxDispImm8 { .. } => 5,
        Instr::MovReg8GroupSymBxDisp { .. } => 4,
        Instr::MovGroupSymBxDispReg8 { .. } => 4,
        Instr::MovGroupSymBxDispImm8 { .. } => 5,
        Instr::AddReg16GroupSym { .. } => 4,
        Instr::OrReg16GroupSym { .. } => 4,
        Instr::MovGroupSymBxDispImm { .. } => 6,
        Instr::MovGroupSymBxDispReg16 { .. } => 4,
        Instr::MovGroupSymSiDispByteImm8 { .. } => 5,
        Instr::MovGroupSymSiDispReg8 { .. } => 4,
        Instr::MovReg8GroupSymSiDisp { .. } => 4,
        Instr::MovReg16GroupSymSiDisp { .. } => 4,
        Instr::MovGroupSymSiDispReg16 { .. } => 4,
        Instr::MovGroupSymSiDispImm16 { .. } => 6,
        Instr::MovReg16Imm { .. } => 3,
        Instr::SubSpImm(imm) | Instr::AddSpImm(imm) => {
            if i8::try_from(*imm as i16).is_ok() { 3 } else { 4 }
        }
        Instr::MovReg16BpRel { offset, .. }
        | Instr::AddAxBpRel { offset }
        | Instr::AdcDxBpRel { offset }
        | Instr::SbbDxBpRel { offset }
        | Instr::AddDxBpRel { offset }
        | Instr::AdcAxBpRel { offset }
        | Instr::SubDxBpRel { offset }
        | Instr::SbbAxBpRel { offset }
        | Instr::AndDxBpRel { offset }
        | Instr::OrDxBpRel { offset }
        | Instr::XorDxBpRel { offset }
        | Instr::AddBpRelDx { offset }
        | Instr::AddBpRelReg16 { offset, .. }
        | Instr::SubBpRelReg16 { offset, .. }
        | Instr::AndBpRelReg16 { offset, .. }
        | Instr::OrBpRelReg16 { offset, .. }
        | Instr::XorBpRelReg16 { offset, .. }
        | Instr::AdcBpRelAx { offset }
        | Instr::SubBpRelDx { offset }
        | Instr::SbbBpRelAx { offset }
        | Instr::AndBpRelDx { offset }
        | Instr::AndBpRelAx { offset }
        | Instr::OrBpRelDx { offset }
        | Instr::OrBpRelAx { offset }
        | Instr::XorBpRelDx { offset }
        | Instr::XorBpRelAx { offset }
        | Instr::AddBpRelAx { offset }
        | Instr::AdcBpRelDx { offset }
        | Instr::SubBpRelAx { offset }
        | Instr::SbbBpRelDx { offset }
        | Instr::AddBpRelByteAl { offset }
        | Instr::SubBpRelByteAl { offset }
        | Instr::AndBpRelByteAl { offset }
        | Instr::OrBpRelByteAl { offset }
        | Instr::XorBpRelByteAl { offset }
        | Instr::SubAxBpRel { offset }
        | Instr::AndAxBpRel { offset }
        | Instr::AndReg16BpRel { offset, .. }
        | Instr::OrAxBpRel { offset }
        | Instr::OrReg16BpRel { offset, .. }
        | Instr::XorAxBpRel { offset }
        | Instr::XorReg16BpRel { offset, .. }
        | Instr::AddReg16BpRel { offset, .. }
        | Instr::SubReg16BpRel { offset, .. }
        | Instr::CmpAxBpRel { offset }
        | Instr::CmpDxBpRel { offset }
        | Instr::CmpReg16BpRel { offset, .. }
        | Instr::CmpBpRelReg16 { offset, .. }
        | Instr::ImulBpRel { offset }
        | Instr::IdivBpRel { offset }
        | Instr::DivBpRel { offset }
        | Instr::ImulByteBpRel { offset }
        | Instr::IdivByteBpRel { offset }
        | Instr::DivByteBpRel { offset }
        | Instr::MovReg8BpRel { offset, .. }
        | Instr::MovBpRelReg8 { offset, .. } => 1 + bp_rel_modrm_size(*offset),
        Instr::MovReg8Imm8 { .. } => 2,
        Instr::MovReg8Reg8 { .. } => 2,
        Instr::MovBpRelImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::MovByteSiDispImm8 { disp, .. } => if *disp == 0 { 3 } else { 4 },
        Instr::MovReg8ByteSiDisp { disp, .. } => if *disp == 0 { 2 } else { 3 },
        Instr::IncReg8 { .. } | Instr::DecReg8 { .. } => 2,
        Instr::CmpReg8Imm8 { .. } => 3,
        Instr::CmpAlImm8 { .. } => 2,
        Instr::CmpAlBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::AddAlImm8 { .. }
        | Instr::SubAlImm8 { .. }
        | Instr::AndAlImm8 { .. }
        | Instr::OrAlImm8 { .. }
        | Instr::XorAlImm8 { .. } => 2,
        Instr::AndReg8Imm8 { .. }
        | Instr::OrReg8Imm8 { .. }
        | Instr::XorReg8Imm8 { .. } => 3,
        Instr::AddReg8Reg8 { .. }
        | Instr::SubReg8Reg8 { .. }
        | Instr::AndReg8Reg8 { .. }
        | Instr::OrReg8Reg8 { .. }
        | Instr::XorReg8Reg8 { .. } => 2,
        Instr::CallNear(_) => 3,
        Instr::CallFar(_) => 5,
        Instr::MovAxGroupSym { .. }
        | Instr::MovAxSym { .. }
        | Instr::MovAlSym { .. }
        | Instr::MovSymAx { .. }
        | Instr::MovAlGroupSym { .. }
        | Instr::MovGroupSymAl { .. }
        | Instr::MovReg16OffsetGroupSym { .. } => 3,
        Instr::MovReg8GroupSym { .. } => 4,
        Instr::MovReg16WordGroupSym { .. } => 4,
        Instr::MovGroupSymImm16 { .. } => 6,
        Instr::MovSymImm16 { .. } => 6,
        Instr::MovGroupSymImm8 { .. } => 5,
        Instr::MovGroupSymOffsetGroupSym { .. } => 6,
        Instr::MovGroupSymAx { .. } => 3,
        Instr::MovGroupSymReg16 { .. } => 4,
        Instr::MovGroupSymReg8 { .. } => 4,
        Instr::AddReg16Imm8Sx { .. }
        | Instr::AdcReg16Imm8Sx { .. }
        | Instr::SbbReg16Imm8Sx { .. }
        | Instr::SubReg16Imm8Sx { .. } => 3,
        Instr::AddReg16Imm16 { .. } | Instr::SubReg16Imm16 { .. } => 4,
        Instr::OrReg16Imm16 { .. } | Instr::AndReg16Imm16 { .. } | Instr::XorReg16Imm16 { .. } => 4,
        Instr::AddGroupSymImm16 { .. } => 6,
        Instr::AdcAxImm16 { .. }
        | Instr::SbbAxImm16 { .. }
        | Instr::AndAxImm16 { .. }
        | Instr::OrAxImm16 { .. }
        | Instr::XorAxImm16 { .. } => 3,
        Instr::MovAlFromSiPtr | Instr::MovAlFromBxPtr | Instr::MovAlFromDiPtr => 2,
        Instr::MovAlFromBxSi | Instr::MovAlFromBxDi => 2,
        Instr::MovBxSiPtrImm8 { .. } | Instr::MovBxDiPtrImm8 { .. } => 3,
        Instr::ImulReg16 { .. } | Instr::IdivReg16 { .. } | Instr::DivReg16 { .. } => 2,
        Instr::AddAxOffsetGroupSym { .. } => 3,
        Instr::SubAxOffsetGroupSym { .. } => 3,
        Instr::CmpReg16OffsetGroupSym { .. } => 4,
        Instr::AddAxSym { .. } => 4,
        Instr::AddAxGroupSym { .. }
        | Instr::OrAxGroupSym { .. }
        | Instr::AddDxGroupSym { .. }
        | Instr::AdcAxGroupSym { .. }
        | Instr::AddGroupSymDx { .. }
        | Instr::AdcGroupSymAx { .. }
        | Instr::SbbGroupSymAx { .. }
        | Instr::AdcGroupSymDx { .. }
        | Instr::SbbGroupSymDx { .. }
        | Instr::AdcDxGroupSym { .. }
        | Instr::SubDxGroupSym { .. }
        | Instr::SubReg16GroupSym { .. }
        | Instr::SbbAxGroupSym { .. }
        | Instr::AndDxGroupSym { .. }
        | Instr::AndAxGroupSym { .. }
        | Instr::OrDxGroupSym { .. }
        | Instr::XorDxGroupSym { .. }
        | Instr::XorAxGroupSym { .. } => 4,
        Instr::CmpAxGroupSym { .. } | Instr::CmpDxGroupSym { .. } => 4,
        Instr::PushGroupSym { .. } => 4,
        Instr::PushBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::PushSiDisp { .. } => 3,
        Instr::PushSiPtr => 2,
        Instr::PushDs => 1,
        Instr::PushSs => 1,
        Instr::PushCs => 1,
        Instr::PushEs => 1,
        Instr::PopEs => 1,
        Instr::PopDs => 1,
        Instr::Iret => 1,
        Instr::MovReg16Dgroup { .. } => 3,
        Instr::MovReg16SegBase { .. } => 3,
        Instr::MovDsReg16 { .. } => 2,
        Instr::MovReg16SegReg { .. } => 2,
        Instr::MovBpRelSegReg { offset, .. }
        | Instr::LesBxBpRel { offset } => {
            1 + bp_rel_modrm_size(*offset)
        }
        Instr::LesBxGroupSym { .. } | Instr::LesBxSym { .. } => 4,
        Instr::MovAxEsBx
        | Instr::MovAlEsBx
        | Instr::MovEsBxAx
        | Instr::MovEsBxAl => 3,
        Instr::MovAxEsBxDisp { .. } | Instr::MovAlEsBxDisp { .. } => 4,
        Instr::MovEsBxImm16 { .. } => 5,
        Instr::MovEsBxImm8 { .. } => 4,
        Instr::MovEsBxDispImm16 { .. } => 6,
        Instr::MovEsBxDispImm8 { .. } => 5,
        Instr::MovEsBxDispAx { .. } => 4,
        Instr::MovEsBxDispAl { .. } => 4,
        Instr::CmpGroupSymImm8Sx { .. }
        | Instr::CmpByteGroupSymImm8 { .. }
        | Instr::AddGroupSymImm8Sx { .. }
        | Instr::AdcGroupSymImm8Sx { .. }
        | Instr::SubGroupSymImm8Sx { .. }
        | Instr::SbbGroupSymImm8Sx { .. } => 5,
        Instr::IncGroupSym { .. } | Instr::DecGroupSym { .. } => 4,
        Instr::IncSym { .. } | Instr::DecSym { .. } => 4,
        Instr::AddSymImm8Sx { .. } | Instr::SubSymImm8Sx { .. } => 5,
        Instr::TestGroupSymImm16 { .. } => 6,
        Instr::TestBpRelImm16 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::TestBpRelAx { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::TestReg16Imm16 { .. } => 4,
        Instr::TestReg16Reg16 { .. } => 2,
        Instr::AddGroupSymReg16 { .. } | Instr::SubGroupSymReg16 { .. } => 4,
        Instr::AndGroupSymReg16 { .. }
        | Instr::OrGroupSymReg16 { .. }
        | Instr::XorGroupSymReg16 { .. } => 4,
        Instr::AddGroupSymReg8 { .. }
        | Instr::SubGroupSymReg8 { .. }
        | Instr::AndGroupSymReg8 { .. }
        | Instr::OrGroupSymReg8 { .. }
        | Instr::XorGroupSymReg8 { .. } => 4,
        Instr::AndGroupSymImm8 { .. }
        | Instr::OrGroupSymImm8 { .. }
        | Instr::XorGroupSymImm8 { .. } => 5,
        Instr::IncBpRel { .. } | Instr::DecBpRel { .. } => 3,
        Instr::ShlGroupSymOne { .. }
        | Instr::SarGroupSymOne { .. }
        | Instr::ShrGroupSymOne { .. } => 4,
        Instr::ShlGroupSymByteOne { .. }
        | Instr::SarGroupSymByteOne { .. }
        | Instr::ShrGroupSymByteOne { .. } => 4,
        Instr::ShlGroupSymByteCl { .. }
        | Instr::SarGroupSymByteCl { .. }
        | Instr::ShrGroupSymByteCl { .. } => 4,
        Instr::ShlGroupSymCl { .. }
        | Instr::SarGroupSymCl { .. }
        | Instr::ShrGroupSymCl { .. } => 4,
        Instr::ImulGroupSym { .. } | Instr::IdivGroupSym { .. } => 4,
        Instr::ImulSiPtr | Instr::IdivSiPtr => 2,
        Instr::IncGroupSymByte { .. } | Instr::DecGroupSymByte { .. } => 4,
        Instr::IncBpRelByte { offset } | Instr::DecBpRelByte { offset } => {
            1 + bp_rel_modrm_size(*offset)
        }
        Instr::CmpByteBpRelImm8 { .. } => 4,
        Instr::CmpByteSiPtrImm8 { .. } | Instr::CmpByteBxPtrImm8 { .. } | Instr::CmpByteDiPtrImm8 { .. } => 3,
        Instr::CmpWordSiPtrImm8Sx { .. } | Instr::CmpWordDiPtrImm8Sx { .. } | Instr::CmpWordBxPtrImm8Sx { .. } => 3,
        Instr::CmpWordSiPtrImm16 { .. } | Instr::CmpWordDiPtrImm16 { .. } | Instr::CmpWordBxPtrImm16 { .. } => 4,
        Instr::CmpAxFromDiPtr
        | Instr::CmpAxFromSiPtr
        | Instr::CmpAxFromBxPtr
        | Instr::CmpAlFromSiPtr
        | Instr::CmpAlFromDiPtr
        | Instr::CmpAlFromBxPtr => 2,
        Instr::CmpWordSiDispImm8Sx { disp, .. } => if *disp == 0 { 3 } else { 4 },
        Instr::AndGroupSymImm16 { .. }
        | Instr::OrGroupSymImm16 { .. }
        | Instr::XorGroupSymImm16 { .. }
        | Instr::CmpGroupSymImm16 { .. } => 6,
        Instr::CmpGroupSymReg16 { .. } => 4,
        Instr::Cbw => 1,
        Instr::LeaReg16BpRel { offset, .. } => 1 + bp_rel_modrm_size(*offset),
        Instr::MovSiPtrImm { .. } | Instr::MovBxPtrImm { .. } | Instr::MovDiPtrImm { .. } => 4,
        Instr::MovBxPtrImm8 { .. } => 3,
        Instr::AddSiPtrImm16 { .. } => 4,
        Instr::XorDiPtrReg16 { .. } => 2,
        Instr::MovBxPtrAl | Instr::MovBxPtrAx => 2,
        Instr::MovBxPtrReg16 { .. } => 2,
        Instr::MovSiPtrImm8 { .. } => 3,
        Instr::MovSiPtrReg16 { .. } | Instr::MovDiPtrReg16 { .. } => 2,
        Instr::MovSiPtrReg8 { .. } | Instr::MovDiPtrReg8 { .. } => 2,
        Instr::MovSiDispImm { .. } => 5,
        Instr::MovSiDispReg16 { .. } => 3,
        Instr::MovAxSiDisp { .. } | Instr::MovDxSiDisp { .. } => 3,
        Instr::MovDxFromSiPtr => 2,
        Instr::MovReg16FromSiPtr { .. } => 2,
        Instr::MovReg16SiDisp { .. } => 3,
        Instr::MovReg16FromDiPtr { .. } => 2,
        Instr::MovReg16DiDisp { .. } => 3,
        Instr::MovReg16FromBxPtr { .. } => 2,
        Instr::AddSiPtrImm8 { .. } | Instr::AddBxPtrImm8 { .. } | Instr::SubSiPtrImm8 { .. } => 3,
        Instr::AndSiPtrByteImm8 { .. }
        | Instr::OrSiPtrByteImm8 { .. }
        | Instr::XorSiPtrByteImm8 { .. } => 3,
        Instr::AndBpRelByteImm8 { offset, .. }
        | Instr::OrBpRelByteImm8 { offset, .. }
        | Instr::XorBpRelByteImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::AddSiPtrReg8 { .. } | Instr::SubSiPtrReg8 { .. } => 2,
        Instr::IncSiPtrByte | Instr::DecSiPtrByte => 2,
        Instr::IncSiPtrWord | Instr::DecSiPtrWord => 2,
        Instr::AdcSiDispImm8 { .. } | Instr::SbbSiDispImm8 { .. } => 4,
        Instr::AddSiPtrDx => 2,
        Instr::AddSiPtrAx
        | Instr::SubSiPtrAx
        | Instr::AndSiPtrAx
        | Instr::OrSiPtrAx
        | Instr::XorSiPtrAx
        | Instr::ShlSiPtrCl
        | Instr::SarSiPtrCl
        | Instr::ShrSiPtrCl => 2,
        Instr::AddBxDispAx { .. }
        | Instr::SubBxDispAx { .. }
        | Instr::AndBxDispAx { .. }
        | Instr::OrBxDispAx { .. }
        | Instr::XorBxDispAx { .. } => 3,
        Instr::AddSiDispAx { .. }
        | Instr::SubSiDispAx { .. }
        | Instr::AndSiDispAx { .. }
        | Instr::OrSiDispAx { .. }
        | Instr::XorSiDispAx { .. } => 3,
        Instr::AddBxDispImm8 { .. } | Instr::SubBxDispImm8 { .. } => 4,
        Instr::MovAlBxDisp { .. } | Instr::MovBxDispAl { .. } => 3,
        Instr::AndBxDispAl { .. }
        | Instr::OrBxDispAl { .. }
        | Instr::XorBxDispAl { .. } => 3,
        Instr::AndBxDispImm16 { .. }
        | Instr::OrBxDispImm16 { .. }
        | Instr::XorBxDispImm16 { .. } => 5,
        Instr::AddBxPtrAx
        | Instr::SubBxPtrAx
        | Instr::AndBxPtrAx
        | Instr::OrBxPtrAx
        | Instr::XorBxPtrAx => 2,
        Instr::IncBxDisp { .. } | Instr::DecBxDisp { .. } => 3,
        Instr::IncBxDispByte { .. } | Instr::DecBxDispByte { .. } => 3,
        Instr::CmpBxDispImm8 { .. } => 4,
        Instr::ShlBxDispImm1 { .. }
        | Instr::SarBxDispImm1 { .. }
        | Instr::ShrBxDispImm1 { .. } => 3,
        Instr::ShlBxDispCl { .. }
        | Instr::SarBxDispCl { .. }
        | Instr::ShrBxDispCl { .. } => 3,
        Instr::MovAxBxDisp { .. } | Instr::MovBxDispAx { .. } => 3,
        Instr::MovBxDispDx { .. } => 3,
        Instr::MovDxBxDisp { .. } => 3,
        Instr::MovBxBxDisp { .. } => 3,
        Instr::MovBxDispImm { .. } => 5,
        Instr::AdcBxDispImm8 { .. } | Instr::SbbBxDispImm8 { .. } => 4,
        Instr::PushBxDisp { .. } => 3,
        Instr::AddAlBpRel { offset }
        | Instr::AddClBpRel { offset }
        | Instr::SubAlBpRel { offset }
        | Instr::AndAlBpRel { offset }
        | Instr::OrAlBpRel { offset }
        | Instr::XorAlBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::AdcSiDispAx { .. } => 3,
        Instr::AdcSiDispDx { .. } | Instr::SbbSiDispDx { .. } => 3,
        Instr::AddBpRelImm8 { offset, .. }
        | Instr::AdcBpRelImm8 { offset, .. }
        | Instr::SubBpRelImm8 { offset, .. }
        | Instr::SbbBpRelImm8 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 1,
        Instr::AndBpRelImm16 { offset, .. }
        | Instr::OrBpRelImm16 { offset, .. }
        | Instr::XorBpRelImm16 { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::MovAxFromSiPtr
        | Instr::MovAxFromBxPtr
        | Instr::MovBxFromBxPtr
        | Instr::SubAxFromSiPtr
        | Instr::SubAxFromDiPtr
        | Instr::AddAxFromSiPtr
        | Instr::AddAxFromDiPtr
        | Instr::AddReg16FromBxPtr { .. }
        | Instr::AddReg16FromDiPtr { .. }
        | Instr::AddReg16FromSiPtr { .. } => 2,
        Instr::AddAxSiDisp { .. }
        | Instr::AddAxDiDisp { .. }
        | Instr::AddAxBxDisp { .. }
        | Instr::SubAxSiDisp { .. }
        | Instr::SubAxDiDisp { .. }
        | Instr::AddReg16SiDisp { .. }
        | Instr::AddReg16DiDisp { .. } => 3,
        Instr::ShlReg16One { .. }
        | Instr::RclReg16One { .. }
        | Instr::SarReg16One { .. }
        | Instr::ShrReg16One { .. }
        | Instr::RcrReg16One { .. }
        | Instr::NegReg16 { .. }
        | Instr::NotReg16 { .. }
        | Instr::ShlReg8One { .. }
        | Instr::SarReg8One { .. }
        | Instr::ShrReg8One { .. } => 2,
        Instr::ShlReg16Imm8 { .. } => 3,
        Instr::MovBpRelImm { offset, .. }
        | Instr::MovBpRelOffsetSym { offset, .. }
        | Instr::MovBpRelOffsetGroupSym { offset, .. } => 1 + bp_rel_modrm_size(*offset) + 2,
        Instr::MovDerefRegOffsetGroupSym { .. } => 4,
        Instr::CallIndirectBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::CallFarIndirectBpRel { offset } => 1 + bp_rel_modrm_size(*offset),
        Instr::CallIndirectGroupSym { .. } => 4,
        Instr::CallIndirectGroupSymBx { .. } => 4,
        Instr::CallIndirectBx => 2,
        // 8087 FPU memory ops: TASM auto-prepends a `9B` (FWAIT)
        // prefix before each memory-form FPU instruction (matches
        // real TASM's 8087 compatibility behavior). The family
        // opcode (D9/DD) + ModR/M (+ disp) follow. Total = 1
        // (fwait) + 1 (family) + ModR/M + disp; group-symbol forms
        // are always disp16 (5 bytes total with the fwait).
        Instr::FldDwordBpRel { offset }
        | Instr::FstpDwordBpRel { offset }
        | Instr::FldQwordBpRel { offset }
        | Instr::FstpQwordBpRel { offset }
        | Instr::FpuArithBpRel { offset, .. }
        | Instr::FildWordBpRel { offset }
        | Instr::FcompBpRel { offset, .. }
        | Instr::FstswWordBpRel { offset } => 2 + bp_rel_modrm_size(*offset),
        Instr::FldDwordGroupSym { .. } | Instr::FldQwordGroupSym { .. }
        | Instr::FstpDwordGroupSym { .. } | Instr::FstpQwordGroupSym { .. }
        | Instr::FcompGroupSym { .. } | Instr::FpuArithGroupSym { .. } => 5,
        Instr::FldDwordGroupSymBx { .. } | Instr::FldQwordGroupSymBx { .. } => 5,
        // Register-form FPU instructions: 9B (fwait) + family +
        // register-mode ModR/M = 3 bytes flat. No memory displacement.
        Instr::Fld1 | Instr::FsubpStack | Instr::Fchs | Instr::Fldz | Instr::Fcompp => 3,
        // Standalone fwait emits `90 9B` (NOP + FWAIT) — TASM tags
        // the NOP byte with the FIWRQQ marker. 2 bytes total.
        Instr::Fwait => 2,
        Instr::Sahf => 1,
    }
}
