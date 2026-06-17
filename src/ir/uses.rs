//! Per-instruction operand shape: the VarIds an instruction or terminator
//! reads, and the VarId it defines.
//!
//! Several passes need this (DCE, tail-call detection, liveness). Keeping the
//! one authoritative `match` here means a new `Instruction`/`Terminator` shape
//! only has to be taught in one place. Total over all variants, including the
//! pre-SSA `Assign`/`Read` (so callers that run before `ssa::promote` are safe).

use crate::ir::{Instruction, Terminator, VarId};

/// VarIds read (used) by an instruction. Does not include the dest.
pub(crate) fn instruction_reads(inst: &Instruction) -> Vec<VarId> {
    match inst {
        Instruction::Phi { sources, .. } => sources.iter().map(|(_, v)| *v).collect(),
        Instruction::Copy { src, .. } => vec![*src],
        Instruction::Const { .. } => vec![],
        Instruction::Index { base, key, .. } => vec![*base, *key],
        Instruction::Intrinsic { args, .. } => args.clone(),
        Instruction::Call { args, .. } => args.clone(),
        Instruction::MakeAccessor { base, key, .. } => vec![*base, *key],
        Instruction::MakeRef { base, .. } => vec![*base],
        Instruction::WriteRef { ref_var, value } => vec![*ref_var, *value],
        Instruction::WriteAccessor { base, key, value } => vec![*base, *key, *value],
        Instruction::Append { arr, value, .. } => vec![*arr, *value],
        Instruction::Reload { src, .. } => vec![*src],
        Instruction::LoadGlobal { .. } => vec![],
        Instruction::StoreGlobal { value, .. } => vec![*value],
        Instruction::Assign { value, .. } => vec![*value],
        Instruction::Read { .. } => vec![],
    }
}

/// VarIds read (used) by a terminator.
pub(crate) fn terminator_reads(term: &Terminator) -> Vec<VarId> {
    match term {
        Terminator::If { condition, .. } => vec![*condition],
        Terminator::Match { value, .. } => vec![*value],
        Terminator::Return { value: Some(v) } => vec![*v],
        Terminator::TailCall { args, .. } => args.clone(),
        Terminator::Jump { .. } | Terminator::Return { value: None } | Terminator::Unreachable => {
            vec![]
        }
    }
}

/// The VarId an instruction defines, if any. Side-effecting writes
/// (`WriteRef`/`WriteAccessor`/`StoreGlobal`) and pre-SSA `Assign` define no
/// VarId; pre-SSA `Read` defines its dest.
pub(crate) fn instruction_dest(inst: &Instruction) -> Option<VarId> {
    match inst {
        Instruction::Const { dest, .. }
        | Instruction::Copy { dest, .. }
        | Instruction::Index { dest, .. }
        | Instruction::Intrinsic { dest, .. }
        | Instruction::Phi { dest, .. }
        | Instruction::MakeAccessor { dest, .. }
        | Instruction::MakeRef { dest, .. }
        | Instruction::Append { dest, .. }
        | Instruction::Call { dest, .. }
        | Instruction::Reload { dest, .. }
        | Instruction::LoadGlobal { dest, .. }
        | Instruction::Read { dest, .. } => Some(*dest),

        Instruction::WriteRef { .. }
        | Instruction::WriteAccessor { .. }
        | Instruction::StoreGlobal { .. }
        | Instruction::Assign { .. } => None,
    }
}
