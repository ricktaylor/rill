//! CFG Simplification
//!
//! Simplifies the control flow graph after optimization passes by:
//! 1. Removing unreachable blocks (no predecessors except entry)
//! 2. Merging single-predecessor/single-successor block chains

use crate::ir::{BlockId, Function, Instruction, Terminator, VarId, cfg};
use std::collections::{HashMap, HashSet};

// ============================================================================
// CFG Simplification
// ============================================================================

/// Simplify the CFG after optimization passes.
///
/// Performs:
/// 1. Remove unreachable blocks (no predecessors except entry)
/// 2. Merge single-predecessor/single-successor block chains
///
/// Returns the number of blocks removed
pub fn simplify_cfg(function: &mut Function) -> usize {
    let initial_count = function.blocks.len();

    // Phase 1: Remove unreachable blocks
    remove_unreachable_blocks(function);

    // Phase 2: Thread jumps — redirect terminators that target empty
    // jump-only blocks to the final destination. This handles blocks
    // created by guard emissions or Match dispatch that ended up empty
    // after optimization (e.g., B1: Jump B4 → redirect Match arm to B4).
    thread_jumps(function);

    // Phase 3: Merge block chains
    merge_block_chains(function);

    // Phase 4: Remove unreachable blocks again (merging may create more)
    remove_unreachable_blocks(function);

    initial_count - function.blocks.len()
}

/// Redirect terminators that target empty jump-only blocks to the final
/// destination. Follows chains (A→B→C where B and C are both empty jumps).
///
/// A block is "trivial" if it has no instructions (or only Phi instructions
/// with no sources) and its terminator is an unconditional Jump.
fn thread_jumps(function: &mut Function) {
    // Build a map of trivial block redirections: block_id → final target
    let mut redirects: HashMap<BlockId, BlockId> = HashMap::new();

    for block in &function.blocks {
        if let Terminator::Jump { target } = &block.terminator {
            let is_trivial = block.instructions.iter().all(
                |inst| matches!(&inst.node, Instruction::Phi { sources, .. } if sources.is_empty()),
            );
            if is_trivial {
                redirects.insert(block.id, *target);
            }
        }
    }

    if redirects.is_empty() {
        return;
    }

    // Resolve chains: if A→B and B→C, then A→C
    let keys: Vec<BlockId> = redirects.keys().copied().collect();
    for key in keys {
        let mut target = redirects[&key];
        let mut visited = HashSet::new();
        visited.insert(key);
        while let Some(&next) = redirects.get(&target) {
            if !visited.insert(target) {
                break; // cycle
            }
            target = next;
        }
        redirects.insert(key, target);
    }

    // Build reverse map: for each trivial block, which NON-TRIVIAL blocks
    // reach it? We skip trivial blocks as predecessors and follow chains,
    // so A→B→C→D (B, C trivial) records incoming[B]=[A], incoming[C]=[A].
    // This ensures Phi sources are rewritten to the actual predecessors
    // that will target the final destination after threading.
    let mut incoming: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for block in &function.blocks {
        if redirects.contains_key(&block.id) {
            continue;
        }
        for succ in block.terminator.successors() {
            let mut t = succ;
            while redirects.contains_key(&t) {
                incoming.entry(t).or_default().push(block.id);
                t = redirects[&t];
            }
        }
    }

    // Check for conflicts: threading would be unsafe if it creates a Phi
    // with two sources from the same predecessor carrying different values.
    // This happens when an If/Match has both branches go through different
    // trivial blocks to the same join — the Phi uses block identity to
    // distinguish which edge was taken.
    let mut unsafe_redirects: HashSet<BlockId> = HashSet::new();
    for block in &function.blocks {
        for inst in &block.instructions {
            if let Instruction::Phi { sources, .. } = &inst.node {
                // Simulate the rewrite and check for conflicting predecessors
                let mut pred_values: HashMap<BlockId, VarId> = HashMap::new();
                for (src_block, var) in sources {
                    if let Some(preds) = incoming.get(src_block) {
                        for &pred in preds {
                            if let Some(&existing) = pred_values.get(&pred) {
                                if existing != *var {
                                    // Conflict: same predecessor, different values.
                                    // Don't thread this trivial block.
                                    unsafe_redirects.insert(*src_block);
                                }
                            } else {
                                pred_values.insert(pred, *var);
                            }
                        }
                    } else {
                        pred_values.insert(*src_block, *var);
                    }
                }
            }
        }
    }

    // Remove unsafe redirects
    for block_id in &unsafe_redirects {
        redirects.remove(block_id);
        incoming.remove(block_id);
    }

    if redirects.is_empty() {
        return;
    }

    // Rewrite Phi sources BEFORE rewriting terminators: Phis in the final
    // target that reference the trivial block must be expanded to reference
    // all predecessors that originally targeted the trivial block.
    for block in &mut function.blocks {
        for inst in &mut block.instructions {
            if let Instruction::Phi { sources, .. } = &mut inst.node {
                let mut new_sources = Vec::new();
                for (src_block, var) in sources.drain(..) {
                    if let Some(preds) = incoming.get(&src_block) {
                        for &pred in preds {
                            new_sources.push((pred, var));
                        }
                    } else {
                        new_sources.push((src_block, var));
                    }
                }
                *sources = new_sources;
            }
        }
    }

    // Rewrite all terminators
    for block in &mut function.blocks {
        rewrite_terminator_targets(&mut block.terminator, &redirects);
    }
}

/// Rewrite all block targets in a terminator using the redirect map.
fn rewrite_terminator_targets(terminator: &mut Terminator, redirects: &HashMap<BlockId, BlockId>) {
    match terminator {
        Terminator::Jump { target } => {
            if let Some(&new) = redirects.get(target) {
                *target = new;
            }
        }
        Terminator::If {
            then_target,
            else_target,
            ..
        } => {
            if let Some(&new) = redirects.get(then_target) {
                *then_target = new;
            }
            if let Some(&new) = redirects.get(else_target) {
                *else_target = new;
            }
        }
        Terminator::Match { arms, default, .. } => {
            for (_, target) in arms.iter_mut() {
                if let Some(&new) = redirects.get(target) {
                    *target = new;
                }
            }
            if let Some(&new) = redirects.get(default) {
                *default = new;
            }
        }
        Terminator::Return { .. } | Terminator::Unreachable | Terminator::TailCall { .. } => {}
    }
}

/// Remove blocks that have no predecessors (except the entry block)
fn remove_unreachable_blocks(function: &mut Function) {
    // Compute reachable blocks from entry. Scope the block map so its borrow
    // of `function` ends before the mutable `retain` below.
    let reachable = {
        let block_map = cfg::block_map(function);
        cfg::reachable_blocks(function, &block_map)
    };

    // Remove unreachable blocks
    function.blocks.retain(|b| reachable.contains(&b.id));

    // Clean up phi sources that reference removed blocks
    for block in &mut function.blocks {
        for inst in &mut block.instructions {
            if let Instruction::Phi { sources, .. } = &mut inst.node {
                sources.retain(|(block_id, _)| reachable.contains(block_id));
            }
        }
    }

    simplify_phis(function);
}

/// Simplify Phi instructions:
/// 1. Single-source Phi → Copy (source lost to unreachable block removal)
/// 2. All-same-source Phi → Copy (all predecessors provide the same value)
/// 3. Duplicate Phis → Copy of first (identical sources in the same block)
fn simplify_phis(function: &mut Function) {
    for block in &mut function.blocks {
        // Pass 1: collapse trivial Phis to Copies
        for inst in &mut block.instructions {
            let replacement = match &inst.node {
                Instruction::Phi { dest, sources } if sources.len() == 1 => {
                    Some(Instruction::Copy {
                        dest: *dest,
                        src: sources[0].1,
                    })
                }
                Instruction::Phi { dest, sources }
                    if sources.len() > 1 && sources.iter().all(|(_, v)| *v == sources[0].1) =>
                {
                    Some(Instruction::Copy {
                        dest: *dest,
                        src: sources[0].1,
                    })
                }
                _ => None,
            };
            if let Some(new_inst) = replacement {
                inst.node = new_inst;
            }
        }

        // Pass 2: deduplicate identical Phis within the same block.
        // If two Phis have the same sources, the second becomes a Copy of the first.
        let mut seen_phis: HashMap<Vec<(BlockId, VarId)>, VarId> = HashMap::new();
        for inst in &mut block.instructions {
            let replacement = match &inst.node {
                Instruction::Phi { dest, sources } if !sources.is_empty() => {
                    if let Some(&first_dest) = seen_phis.get(sources) {
                        Some(Instruction::Copy {
                            dest: *dest,
                            src: first_dest,
                        })
                    } else {
                        seen_phis.insert(sources.clone(), *dest);
                        None
                    }
                }
                _ => None,
            };
            if let Some(new_inst) = replacement {
                inst.node = new_inst;
            }
        }
    }
}

/// Merge chains of blocks where one has a single successor and the other
/// has a single predecessor
fn merge_block_chains(function: &mut Function) {
    // Build predecessor map
    let mut predecessors: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
    for block in &function.blocks {
        for succ in block.terminator.successors() {
            predecessors.entry(succ).or_default().push(block.id);
        }
    }

    // Find merge candidates: blocks with single predecessor where that
    // predecessor has a single successor (unconditional jump)
    let mut merged = HashSet::new();

    loop {
        let mut found_merge = false;

        for i in 0..function.blocks.len() {
            let block_id = function.blocks[i].id;

            // Skip already merged blocks
            if merged.contains(&block_id) {
                continue;
            }

            // Check if this block has exactly one predecessor
            let preds = predecessors.get(&block_id).cloned().unwrap_or_default();
            if preds.len() != 1 {
                continue;
            }

            let pred_id = preds[0];

            // Skip if predecessor is already merged
            if merged.contains(&pred_id) {
                continue;
            }

            // Skip self-loops
            if pred_id == block_id {
                continue;
            }

            // Check if predecessor has unconditional jump to this block
            let Some(pred_idx) = function.blocks.iter().position(|b| b.id == pred_id) else {
                continue;
            };
            if !matches!(
                &function.blocks[pred_idx].terminator,
                Terminator::Jump { target } if *target == block_id
            ) {
                continue;
            }

            // Can merge! Append this block's instructions to predecessor
            // and take its terminator
            let block_instructions = std::mem::take(&mut function.blocks[i].instructions);
            let block_terminator =
                std::mem::replace(&mut function.blocks[i].terminator, Terminator::Unreachable);

            function.blocks[pred_idx]
                .instructions
                .extend(block_instructions);
            function.blocks[pred_idx].terminator = block_terminator;

            merged.insert(block_id);
            found_merge = true;

            // Update predecessor map for the merged block's successors
            for succ in function.blocks[pred_idx].terminator.successors() {
                if let Some(succ_preds) = predecessors.get_mut(&succ) {
                    // Replace block_id with pred_id in successor's predecessors
                    for p in succ_preds.iter_mut() {
                        if *p == block_id {
                            *p = pred_id;
                        }
                    }
                }
            }

            // Update phi sources in ALL blocks: replace block_id with pred_id
            for block in function.blocks.iter_mut() {
                for inst in &mut block.instructions {
                    if let Instruction::Phi { sources, .. } = &mut inst.node {
                        for (src_block, _) in sources.iter_mut() {
                            if *src_block == block_id {
                                *src_block = pred_id;
                            }
                        }
                    }
                }
            }

            break; // Restart to handle cascading merges
        }

        if !found_merge {
            break;
        }
    }

    // Remove merged blocks
    function.blocks.retain(|b| !merged.contains(&b.id));

    // Convert phis that became fully self-referencing after merges.
    //
    // When block B is merged into predecessor A, phis from B that had source
    // (A, val) end up as self-referencing: the phi is in A with source (A, val).
    // The compiler handles phis by inserting copies into predecessor blocks.
    // When a phi's source block IS its own block, the copy is inserted alongside
    // other phi copies for downstream phis, creating ordering hazards (the
    // downstream copy reads the stale value before the self-ref copy updates it).
    //
    // Fix: convert fully self-referencing phis to Copy instructions, which
    // execute in the block's normal instruction sequence (before any phi copies).
    for block in &mut function.blocks {
        let block_id = block.id;
        for inst in &mut block.instructions {
            let replacement = match &inst.node {
                Instruction::Phi { dest, sources }
                    if !sources.is_empty()
                        && sources.iter().all(|(src_block, _)| *src_block == block_id) =>
                {
                    // All sources are self-referencing — convert to Copy.
                    // The value was computed earlier in this block (from a merged predecessor).
                    Some(Instruction::Copy {
                        dest: *dest,
                        src: sources[0].1,
                    })
                }
                _ => None,
            };
            if let Some(new_inst) = replacement {
                inst.node = new_inst;
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Instruction, Literal, SpannedInst, VarId};

    fn var(id: u32) -> VarId {
        VarId(id)
    }

    fn block(id: u32) -> BlockId {
        BlockId(id)
    }

    fn si(inst: Instruction) -> SpannedInst {
        ast::Spanned::new(inst, ast::Span::default())
    }

    fn make_function(blocks: Vec<BasicBlock>) -> Function {
        Function {
            blocks,
            ..Default::default()
        }
    }

    // ========================================================================
    // CFG Simplification Tests
    // ========================================================================

    #[test]
    fn test_remove_unreachable_blocks() {
        // Block 2 is unreachable
        //
        // Block 0: Jump B1
        // Block 1: Return
        // Block 2: Return (unreachable)
        //
        // After simplification:
        // - Block 2 removed (unreachable)
        // - Block 0 and Block 1 merged (single pred/succ chain)
        // Result: 1 block remaining
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::Jump { target: block(1) },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function(blocks);
        let removed = simplify_cfg(&mut func);

        // 2 blocks removed: Block 2 (unreachable) + Block 1 (merged into Block 0)
        assert_eq!(removed, 2);
        assert_eq!(func.blocks.len(), 1);
        assert!(!func.blocks.iter().any(|b| b.id == block(2)));
    }

    #[test]
    fn test_merge_block_chain() {
        // Block 0 -> Block 1 (single pred/succ) should merge
        //
        // Block 0: v0 = 1; Jump B1
        // Block 1: v1 = 2; Return
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                })],
                terminator: Terminator::Jump { target: block(1) },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                })],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
        ];

        let mut func = make_function(blocks);
        let removed = simplify_cfg(&mut func);

        assert_eq!(removed, 1);
        assert_eq!(func.blocks.len(), 1);
        assert_eq!(func.blocks[0].instructions.len(), 2);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Return { .. }
        ));
    }

    #[test]
    fn test_no_merge_multiple_predecessors() {
        // Block 1 has two predecessors, shouldn't merge
        //
        // Block 0: If v0 -> B1, B1
        // Block 1: Return
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Bool(true),
                })],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(1),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function(blocks);
        let removed = simplify_cfg(&mut func);

        // No blocks removed - B1 has multiple predecessors (both branches of If)
        assert_eq!(removed, 0);
        assert_eq!(func.blocks.len(), 2);
    }

    #[test]
    fn test_cascade_merge() {
        // Chain of 3 blocks should all merge
        //
        // Block 0: Jump B1
        // Block 1: Jump B2
        // Block 2: Return
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                })],
                terminator: Terminator::Jump { target: block(1) },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                })],
                terminator: Terminator::Jump { target: block(2) },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Const {
                    dest: var(2),
                    value: Literal::UInt(3),
                })],
                terminator: Terminator::Return {
                    value: Some(var(2)),
                },
            },
        ];

        let mut func = make_function(blocks);
        let removed = simplify_cfg(&mut func);

        assert_eq!(removed, 2);
        assert_eq!(func.blocks.len(), 1);
        assert_eq!(func.blocks[0].instructions.len(), 3);
    }
}
