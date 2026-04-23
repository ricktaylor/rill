//! Tail-Call Optimization
//!
//! Detects self-recursive tail calls and rewrites them as `TailCall` terminators.
//! A self-tail-call reuses the current stack frame: overwrite params, jump to entry.
//!
//! Detection traces backward from Return through Phi chains to find Call instructions
//! whose results flow only to the return value (no other uses).
//!
//! Restrictions (initial implementation):
//! - Self-recursive only (function calls itself)
//! - All args by-value (no by_ref)
//! - No rest params

use crate::ir::{BlockId, Function, Instruction, Terminator, VarId};
use std::collections::{HashMap, HashSet};

/// Detect and rewrite self-recursive tail calls in a function.
/// Returns the number of tail calls rewritten.
pub fn optimize_tail_calls(function: &mut Function) -> usize {
    // Skip functions with rest params (variadic)
    if function.rest_param.is_some() {
        return 0;
    }

    let self_name = function.name.to_string();
    let candidates = find_tail_call_candidates(function, &self_name);

    if candidates.is_empty() {
        return 0;
    }

    apply_tail_calls(function, candidates)
}

/// A detected tail call site.
struct TailCallCandidate {
    /// Block containing the Call instruction
    call_block_id: BlockId,
    /// Index of the Call instruction within the block
    call_inst_index: usize,
    /// Argument VarIds
    args: Vec<VarId>,
}

/// Find all self-recursive tail call candidates in the function.
///
/// Algorithm:
/// 1. Find Return blocks that return Some(var)
/// 2. Compute "return-only" VarIds: vars used only in the return value chain
/// 3. Any Call whose dest is return-only and targets self is a candidate
fn find_tail_call_candidates(function: &Function, self_name: &str) -> Vec<TailCallCandidate> {
    // Build a use count map: how many times each VarId is referenced
    // (in instructions and terminators, excluding the defining instruction)
    let use_counts = count_var_uses(function);

    // Find return-only vars by tracing backward from Return terminators
    let return_only = find_return_only_vars(function, &use_counts);

    // Find Call instructions whose dest is return-only and targets self
    let mut candidates = Vec::new();

    for block in &function.blocks {
        for (inst_idx, inst) in block.instructions.iter().enumerate() {
            if let Instruction::Call {
                dest,
                function: func_ref,
                args,
            } = &inst.node
            {
                // Must be self-recursive
                if func_ref.namespace.is_some() || func_ref.name.as_ref() != self_name {
                    continue;
                }

                // Dest must be return-only (flows only to Return through Phis)
                if !return_only.contains(dest) {
                    continue;
                }

                // The Call must be the last real instruction in the block
                // (only Phis or nothing after it, before the terminator)
                let is_last_non_phi = block.instructions[inst_idx + 1..]
                    .iter()
                    .all(|i| matches!(i.node, Instruction::Phi { .. }));
                if !is_last_non_phi {
                    continue;
                }

                // The block must end with a Jump (to a join block)
                // or Return (direct tail call)
                match &block.terminator {
                    Terminator::Jump { .. } | Terminator::Return { .. } => {}
                    _ => continue,
                }

                candidates.push(TailCallCandidate {
                    call_block_id: block.id,
                    call_inst_index: inst_idx,
                    args: args.clone(),
                });
            }
        }
    }

    candidates
}

/// Count how many times each VarId is used (read) across the function.
/// This counts uses in instruction operands and terminators, NOT definitions.
fn count_var_uses(function: &Function) -> HashMap<VarId, usize> {
    let mut counts: HashMap<VarId, usize> = HashMap::new();

    for block in &function.blocks {
        for inst in &block.instructions {
            for var in instruction_reads(&inst.node) {
                *counts.entry(var).or_insert(0) += 1;
            }
        }

        for var in terminator_reads(&block.terminator) {
            *counts.entry(var).or_insert(0) += 1;
        }
    }

    counts
}

/// Get all VarIds read by an instruction (excluding the dest).
fn instruction_reads(inst: &Instruction) -> Vec<VarId> {
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
        Instruction::Assign { value, .. } => vec![*value],
        Instruction::Read { .. } => vec![],
    }
}

/// Get all VarIds read by a terminator.
fn terminator_reads(term: &Terminator) -> Vec<VarId> {
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

/// Find all VarIds that are "return-only": their only use is flowing
/// (possibly through Phi chains) to a Return terminator's value.
///
/// A VarId is return-only if:
/// - It's used exactly once, AND
/// - That single use is either:
///   - A Return { value: Some(var) } terminator, OR
///   - A Phi { sources: [...(_, var)...] } whose dest is also return-only
fn find_return_only_vars(
    function: &Function,
    use_counts: &HashMap<VarId, usize>,
) -> HashSet<VarId> {
    let mut return_only: HashSet<VarId> = HashSet::new();

    // Seed: VarIds directly returned by Return terminators (with use count == 1)
    for block in &function.blocks {
        if let Terminator::Return { value: Some(v) } = &block.terminator
            && use_counts.get(v).copied().unwrap_or(0) == 1
        {
            return_only.insert(*v);
        }
    }

    // Iterate: trace backward through Phis
    // A Phi source var is return-only if:
    // - The Phi's dest is return-only
    // - The source var has use count == 1 (only used in this Phi)
    let mut changed = true;
    while changed {
        changed = false;
        for block in &function.blocks {
            for inst in &block.instructions {
                if let Instruction::Phi { dest, sources } = &inst.node {
                    if !return_only.contains(dest) {
                        continue;
                    }
                    for (_, src_var) in sources {
                        if !return_only.contains(src_var)
                            && use_counts.get(src_var).copied().unwrap_or(0) == 1
                        {
                            return_only.insert(*src_var);
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    return_only
}

/// Apply tail call rewrites to the function.
/// Returns the number of rewrites applied.
fn apply_tail_calls(function: &mut Function, candidates: Vec<TailCallCandidate>) -> usize {
    let mut count = 0;

    // Build a lookup for candidates by block ID
    let mut candidate_map: HashMap<BlockId, TailCallCandidate> = HashMap::new();
    for c in candidates {
        candidate_map.insert(c.call_block_id, c);
    }

    for block in &mut function.blocks {
        if let Some(candidate) = candidate_map.remove(&block.id) {
            // Remove the Call instruction
            block.instructions.remove(candidate.call_inst_index);

            // Replace terminator with TailCall
            block.terminator = Terminator::TailCall {
                args: candidate.args,
            };

            count += 1;
        }
    }

    count
}
