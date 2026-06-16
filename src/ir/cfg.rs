//! Shared control-flow-graph utilities over a `Function`'s basic blocks.
//!
//! These back the reachability/lookup needs of several passes (SSA
//! construction in `ssa/`, CFG simplification in `opt/`) so a new
//! `Terminator` shape only has to be taught to `Terminator::successors()`.

use crate::ir::{BasicBlock, BlockId, Function};
use std::collections::{HashMap, HashSet};

/// Map each block id to its block. O(blocks).
pub fn block_map(function: &Function) -> HashMap<BlockId, &BasicBlock> {
    function.blocks.iter().map(|b| (b.id, b)).collect()
}

/// The set of blocks reachable from the entry via forward edges. O(blocks).
///
/// Traversal order is irrelevant — only the resulting set is returned — so a
/// plain stack worklist is used.
pub fn reachable_blocks(
    function: &Function,
    block_map: &HashMap<BlockId, &BasicBlock>,
) -> HashSet<BlockId> {
    let mut reachable = HashSet::new();
    let mut worklist = vec![function.entry_block];
    while let Some(bid) = worklist.pop() {
        if !reachable.insert(bid) {
            continue;
        }
        if let Some(block) = block_map.get(&bid) {
            for succ in block.terminator.successors() {
                if !reachable.contains(&succ) {
                    worklist.push(succ);
                }
            }
        }
    }
    reachable
}
