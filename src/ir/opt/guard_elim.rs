//! Guard Elimination and CFG Simplification (Pass 2)
//!
//! Uses the definedness analysis results to:
//! 1. Replace Guard terminators with unconditional jumps when the guarded
//!    value's definedness is known (Defined or Undefined)
//! 2. Simplify the resulting CFG by merging blocks and removing unreachable code

use super::definedness::{Definedness, DefinednessAnalysis, analyze_definedness};
use super::{BlockId, Function, Instruction, Terminator};
use crate::builtins::BuiltinRegistry;
use std::collections::{HashMap, HashSet};

// ============================================================================
// Guard Elimination
// ============================================================================

/// Eliminate Guards where the guarded value's definedness is known
///
/// Returns the number of Guards eliminated
pub fn eliminate_guards(function: &mut Function, analysis: &DefinednessAnalysis) -> usize {
    let mut eliminated = 0;

    for block in &mut function.blocks {
        if let Terminator::Guard {
            value,
            defined,
            undefined,
            ..
        } = &block.terminator
        {
            // Get the definedness of the guarded value at this block's exit
            let definedness = analysis.get_at_exit(block.id, *value);

            match definedness {
                Definedness::Defined => {
                    // Value is provably defined - jump directly to defined branch
                    block.terminator = Terminator::Jump { target: *defined };
                    eliminated += 1;
                }
                Definedness::Undefined => {
                    // Value is provably undefined - jump directly to undefined branch
                    block.terminator = Terminator::Jump { target: *undefined };
                    eliminated += 1;
                }
                Definedness::MaybeDefined => {
                    // Need runtime check - keep the Guard
                }
            }
        }
    }

    eliminated
}

// ============================================================================
// CFG Simplification
// ============================================================================

/// Simplify the CFG after guard elimination
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

    // Phase 2: Merge block chains
    merge_block_chains(function);

    // Phase 3: Remove unreachable blocks again (merging may create more)
    remove_unreachable_blocks(function);

    initial_count - function.blocks.len()
}

/// Remove blocks that have no predecessors (except the entry block)
fn remove_unreachable_blocks(function: &mut Function) {
    // Compute reachable blocks via BFS from entry
    let mut reachable = HashSet::new();
    let mut worklist = vec![function.entry_block];

    while let Some(block_id) = worklist.pop() {
        if reachable.contains(&block_id) {
            continue;
        }
        reachable.insert(block_id);

        // Find this block and add its successors
        if let Some(block) = function.blocks.iter().find(|b| b.id == block_id) {
            for succ in block.terminator.successors() {
                if !reachable.contains(&succ) {
                    worklist.push(succ);
                }
            }
        }
    }

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
            let pred_idx = function.blocks.iter().position(|b| b.id == pred_id);
            if pred_idx.is_none() {
                continue;
            }

            let pred_idx = pred_idx.unwrap();
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
}

// ============================================================================
// Combined Pass
// ============================================================================

/// Run guard elimination and CFG simplification
///
/// Returns (guards_eliminated, blocks_removed)
pub fn eliminate_guards_and_simplify(
    function: &mut Function,
    builtins: Option<&BuiltinRegistry>,
) -> (usize, usize) {
    // Run definedness analysis
    let analysis = analyze_definedness(function, builtins);

    // Eliminate guards
    let guards_eliminated = eliminate_guards(function, &analysis);

    // Simplify CFG
    let blocks_removed = simplify_cfg(function);

    (guards_eliminated, blocks_removed)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Instruction, Literal, Param, SpannedInst, VarId};

    fn var(id: u32) -> VarId {
        VarId(id)
    }

    fn block(id: u32) -> BlockId {
        BlockId(id)
    }

    /// Helper to wrap an instruction with a default span
    fn si(inst: Instruction) -> SpannedInst {
        ast::Spanned::new(inst, ast::Span::default())
    }

    fn make_function(blocks: Vec<BasicBlock>) -> Function {
        Function {
            blocks,
            ..Default::default()
        }
    }

    fn make_function_with_param(param_var: VarId, blocks: Vec<BasicBlock>) -> Function {
        Function {
            params: vec![Param {
                var: param_var,
                by_ref: false,
            }],
            blocks,
            ..Default::default()
        }
    }

    // ========================================================================
    // Guard Elimination Tests
    // ========================================================================

    #[test]
    fn test_eliminate_guard_on_const() {
        // Guard on a constant should be eliminated (constants are Defined)
        //
        // Block 0: v0 = 42; Guard v0 -> B1, B2
        // Block 1: return v0
        // Block 2: return undefined
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(1),
                    undefined: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Undefined { dest: var(1) })],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
        ];

        let mut func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);
        let eliminated = eliminate_guards(&mut func, &analysis);

        assert_eq!(eliminated, 1);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(1)
        ));
    }

    #[test]
    fn test_eliminate_guard_on_undefined() {
        // Guard on explicit undefined should jump to undefined branch
        //
        // Block 0: v0 = undefined; Guard v0 -> B1, B2
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Undefined { dest: var(0) })],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(1),
                    undefined: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);
        let eliminated = eliminate_guards(&mut func, &analysis);

        assert_eq!(eliminated, 1);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(2)
        ));
    }

    #[test]
    fn test_keep_guard_on_param() {
        // Guard on parameter should be kept (MaybeDefined)
        //
        // Block 0: Guard v0 -> B1, B2
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(1),
                    undefined: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);
        let eliminated = eliminate_guards(&mut func, &analysis);

        assert_eq!(eliminated, 0);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Guard { .. }
        ));
    }

    #[test]
    fn test_eliminate_guard_after_guard() {
        // Second guard on same value after first guard should be eliminated
        //
        // Block 0: Guard v0 -> B1, B2
        // Block 1: Guard v0 -> B3, B4  (v0 is Defined here!)
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(1),
                    undefined: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(3),
                    undefined: block(4),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
            BasicBlock {
                id: block(3),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            BasicBlock {
                id: block(4),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);
        let eliminated = eliminate_guards(&mut func, &analysis);

        // First guard kept (param is MaybeDefined), second eliminated (Defined in B1)
        assert_eq!(eliminated, 1);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Guard { .. }
        ));
        assert!(matches!(
            func.blocks[1].terminator,
            Terminator::Jump { target } if target == block(3)
        ));
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
    fn test_guard_elim_enables_unreachable_removal() {
        // After eliminating guard on const, undefined branch becomes unreachable
        //
        // Block 0: v0 = 42; Guard v0 -> B1, B2
        // Block 1: Return v0
        // Block 2: Return undefined (unreachable after guard elim)
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(1),
                    undefined: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Undefined { dest: var(1) })],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
        ];

        let mut func = make_function(blocks);
        let (guards, blocks_removed) = eliminate_guards_and_simplify(&mut func, None);

        assert_eq!(guards, 1);
        assert!(blocks_removed >= 1); // At least B2 removed, possibly B0+B1 merged
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
