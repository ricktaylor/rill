//! Dominator tree computation using the Cooper-Harvey-Kennedy (2001) algorithm.
//!
//! Provides `DominatorTree::build(function)` which computes immediate dominators
//! for all reachable blocks, and `dominance_frontier()` for SSA phi placement.

use crate::ir::{BasicBlock, BlockId, Function};
use std::collections::{HashMap, HashSet, VecDeque};

/// Dominator tree for a function's control flow graph.
pub struct DominatorTree {
    /// Immediate dominator: idom[b] = the closest strict dominator of b.
    /// Entry block's idom is itself (sentinel).
    idom: HashMap<BlockId, BlockId>,

    /// Blocks in reverse post-order (entry first).
    rpo: Vec<BlockId>,

    /// Predecessor map (reachable blocks only).
    predecessors: HashMap<BlockId, Vec<BlockId>>,

    /// Dominator-tree children, in RPO order (precomputed so `children()`
    /// is O(1) rather than an O(n) scan of `rpo` on every call).
    children: HashMap<BlockId, Vec<BlockId>>,

    entry: BlockId,
}

impl DominatorTree {
    /// Build a dominator tree for the given function.
    pub fn build(function: &Function) -> Self {
        let block_map: HashMap<BlockId, &BasicBlock> =
            function.blocks.iter().map(|b| (b.id, b)).collect();

        // 1. Find reachable blocks via BFS
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(function.entry_block);
        while let Some(bid) = queue.pop_front() {
            if !reachable.insert(bid) {
                continue;
            }
            if let Some(block) = block_map.get(&bid) {
                for succ in block.terminator.successors() {
                    queue.push_back(succ);
                }
            }
        }

        // 2. Compute reverse post-order via iterative DFS
        let rpo = compute_rpo(function.entry_block, &block_map, &reachable);
        let rpo_number: HashMap<BlockId, usize> =
            rpo.iter().enumerate().map(|(i, &b)| (b, i)).collect();

        // 3. Build predecessor map (reachable blocks only). Iterate in RPO
        //    order so each predecessor list is deterministically ordered:
        //    phi operand lists derive from this, and must not depend on
        //    HashSet iteration order (which varies run to run).
        let mut predecessors: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        for &bid in &rpo {
            predecessors.entry(bid).or_default();
        }
        for &bid in &rpo {
            if let Some(block) = block_map.get(&bid) {
                for succ in block.terminator.successors() {
                    if reachable.contains(&succ) {
                        predecessors.entry(succ).or_default().push(bid);
                    }
                }
            }
        }

        // 4. Compute immediate dominators (Cooper-Harvey-Kennedy)
        let idom = compute_idom(function.entry_block, &rpo, &rpo_number, &predecessors);

        // 5. Precompute dominator-tree children, in RPO order. A block b is a
        //    child of idom[b] (every block except the entry, whose idom is the
        //    self-sentinel). Building once here makes children() O(1).
        let mut children: HashMap<BlockId, Vec<BlockId>> = HashMap::new();
        for &b in &rpo {
            if let Some(&d) = idom.get(&b)
                && d != b
            {
                children.entry(d).or_default().push(b);
            }
        }

        DominatorTree {
            idom,
            rpo,
            predecessors,
            children,
            entry: function.entry_block,
        }
    }

    /// Immediate dominator of `block`. Returns `None` for the entry block.
    pub fn idom(&self, block: BlockId) -> Option<BlockId> {
        let dom = *self.idom.get(&block)?;
        if block == self.entry { None } else { Some(dom) }
    }

    /// Does `a` dominate `b`? (a dom b means every path from entry to b
    /// passes through a). A block dominates itself.
    pub fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        let mut current = b;
        loop {
            if current == a {
                return true;
            }
            match self.idom.get(&current) {
                Some(&dom) if dom != current => current = dom,
                _ => return false,
            }
        }
    }

    /// Compute the dominance frontier for all blocks.
    ///
    /// DF(b) = set of blocks where b's dominance ends — the join points
    /// reachable from b's subtree but not strictly dominated by b.
    pub fn dominance_frontier(&self) -> HashMap<BlockId, HashSet<BlockId>> {
        let mut df: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
        for &b in &self.rpo {
            df.entry(b).or_default();
        }

        for &b in &self.rpo {
            let preds = match self.predecessors.get(&b) {
                Some(p) => p,
                None => continue,
            };
            if preds.len() < 2 {
                continue;
            }
            // b is a join point (multiple predecessors).
            // Walk each pred's dominator chain up to idom(b).
            let idom_b = match self.idom.get(&b) {
                Some(&d) => d,
                None => continue,
            };
            for &pred in preds {
                let mut runner = pred;
                while runner != idom_b {
                    df.entry(runner).or_default().insert(b);
                    match self.idom.get(&runner) {
                        Some(&d) if d != runner => runner = d,
                        _ => break,
                    }
                }
            }
        }

        df
    }

    /// Children of `block` in the dominator tree (blocks immediately
    /// dominated by `block`), in RPO order.
    pub fn children(&self, block: BlockId) -> &[BlockId] {
        self.children.get(&block).map_or(&[], |c| c.as_slice())
    }

    /// Predecessor blocks (reachable only). Used by SSA construction
    /// to fill phi operands.
    pub fn predecessors(&self) -> &HashMap<BlockId, Vec<BlockId>> {
        &self.predecessors
    }

    /// Entry block.
    pub fn entry(&self) -> BlockId {
        self.entry
    }
}

/// Compute reverse post-order via iterative DFS.
fn compute_rpo(
    entry: BlockId,
    block_map: &HashMap<BlockId, &BasicBlock>,
    reachable: &HashSet<BlockId>,
) -> Vec<BlockId> {
    // Precompute successors per block (filtered to reachable)
    let succs_map: HashMap<BlockId, Vec<BlockId>> = reachable
        .iter()
        .map(|&bid| {
            let succs = block_map
                .get(&bid)
                .map(|b| {
                    b.terminator
                        .successors()
                        .into_iter()
                        .filter(|s| reachable.contains(s))
                        .collect()
                })
                .unwrap_or_default();
            (bid, succs)
        })
        .collect();

    let mut post_order = Vec::new();
    let mut visited = HashSet::new();

    // Iterative DFS using explicit stack.
    // Each frame is (block, successor_iterator_index).
    let mut stack: Vec<(BlockId, usize)> = vec![(entry, 0)];
    visited.insert(entry);

    while let Some(&mut (block, ref mut idx)) = stack.last_mut() {
        let succs = succs_map.get(&block).map(|s| s.as_slice()).unwrap_or(&[]);

        if *idx < succs.len() {
            let succ = succs[*idx];
            *idx += 1;
            if visited.insert(succ) {
                stack.push((succ, 0));
            }
        } else {
            post_order.push(block);
            stack.pop();
        }
    }

    post_order.reverse();
    post_order
}

/// Cooper-Harvey-Kennedy iterative dominator computation.
fn compute_idom(
    entry: BlockId,
    rpo: &[BlockId],
    rpo_number: &HashMap<BlockId, usize>,
    predecessors: &HashMap<BlockId, Vec<BlockId>>,
) -> HashMap<BlockId, BlockId> {
    let mut idom: HashMap<BlockId, BlockId> = HashMap::new();
    idom.insert(entry, entry);

    let mut changed = true;
    while changed {
        changed = false;
        for &b in rpo.iter().skip(1) {
            let preds = match predecessors.get(&b) {
                Some(p) => p,
                None => continue,
            };

            // Find first processed predecessor
            let mut new_idom = None;
            for &p in preds {
                if idom.contains_key(&p) {
                    new_idom = Some(p);
                    break;
                }
            }

            let mut new_idom = match new_idom {
                Some(n) => n,
                None => continue,
            };

            // Intersect with remaining processed predecessors
            for &p in preds {
                if p == new_idom {
                    continue;
                }
                if idom.contains_key(&p) {
                    new_idom = intersect(p, new_idom, &idom, rpo_number);
                }
            }

            if idom.get(&b) != Some(&new_idom) {
                idom.insert(b, new_idom);
                changed = true;
            }
        }
    }

    idom
}

/// Walk up both fingers until they meet.
fn intersect(
    mut b1: BlockId,
    mut b2: BlockId,
    idom: &HashMap<BlockId, BlockId>,
    rpo_number: &HashMap<BlockId, usize>,
) -> BlockId {
    while b1 != b2 {
        let n1 = rpo_number.get(&b1).copied().unwrap_or(usize::MAX);
        let n2 = rpo_number.get(&b2).copied().unwrap_or(usize::MAX);
        if n1 > n2 {
            b1 = *idom.get(&b1).unwrap_or(&b1);
        } else {
            b2 = *idom.get(&b2).unwrap_or(&b2);
        }
    }
    b1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{Terminator, Var};
    use crate::types::TypeSet;

    fn make_block(id: u32, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            instructions: vec![],
            terminator,
        }
    }

    fn make_function(blocks: Vec<BasicBlock>) -> Function {
        Function {
            name: ast::Identifier("test".to_string()),
            params: vec![],
            rest_param: None,
            locals: vec![Var::new(
                crate::ir::VarId(0),
                ast::Identifier("x".to_string()),
                TypeSet::any(),
            )],
            blocks,
            entry_block: BlockId(0),
        }
    }

    #[test]
    fn test_linear_chain() {
        // A(0) → B(1) → C(2) → return
        let func = make_function(vec![
            make_block(0, Terminator::Jump { target: BlockId(1) }),
            make_block(1, Terminator::Jump { target: BlockId(2) }),
            make_block(2, Terminator::Return { value: None }),
        ]);

        let tree = DominatorTree::build(&func);

        assert_eq!(tree.idom(BlockId(0)), None); // entry
        assert_eq!(tree.idom(BlockId(1)), Some(BlockId(0)));
        assert_eq!(tree.idom(BlockId(2)), Some(BlockId(1)));

        assert!(tree.dominates(BlockId(0), BlockId(2)));
        assert!(tree.dominates(BlockId(1), BlockId(2)));
        assert!(!tree.dominates(BlockId(2), BlockId(0)));
        assert!(tree.dominates(BlockId(1), BlockId(1))); // self
    }

    #[test]
    fn test_diamond() {
        // A(0) → B(1), A(0) → C(2), B(1) → D(3), C(2) → D(3)
        let func = make_function(vec![
            make_block(
                0,
                Terminator::If {
                    condition: crate::ir::VarId(0),
                    then_target: BlockId(1),
                    else_target: BlockId(2),
                    span: ast::Span::default(),
                },
            ),
            make_block(1, Terminator::Jump { target: BlockId(3) }),
            make_block(2, Terminator::Jump { target: BlockId(3) }),
            make_block(3, Terminator::Return { value: None }),
        ]);

        let tree = DominatorTree::build(&func);

        assert_eq!(tree.idom(BlockId(1)), Some(BlockId(0)));
        assert_eq!(tree.idom(BlockId(2)), Some(BlockId(0)));
        assert_eq!(tree.idom(BlockId(3)), Some(BlockId(0)));

        assert!(tree.dominates(BlockId(0), BlockId(3)));
        assert!(!tree.dominates(BlockId(1), BlockId(3)));
        assert!(!tree.dominates(BlockId(2), BlockId(3)));

        let df = tree.dominance_frontier();
        assert!(df[&BlockId(1)].contains(&BlockId(3)));
        assert!(df[&BlockId(2)].contains(&BlockId(3)));
        assert!(df[&BlockId(0)].is_empty());
    }

    #[test]
    fn test_loop() {
        // A(0) → B(1), B(1) → C(2), C(2) → B(1), C(2) → D(3)
        let func = make_function(vec![
            make_block(0, Terminator::Jump { target: BlockId(1) }),
            make_block(1, Terminator::Jump { target: BlockId(2) }),
            make_block(
                2,
                Terminator::If {
                    condition: crate::ir::VarId(0),
                    then_target: BlockId(1),
                    else_target: BlockId(3),
                    span: ast::Span::default(),
                },
            ),
            make_block(3, Terminator::Return { value: None }),
        ]);

        let tree = DominatorTree::build(&func);

        assert_eq!(tree.idom(BlockId(1)), Some(BlockId(0)));
        assert_eq!(tree.idom(BlockId(2)), Some(BlockId(1)));
        assert_eq!(tree.idom(BlockId(3)), Some(BlockId(2)));

        let df = tree.dominance_frontier();
        // B(1) is in DF(C(2)) because C→B is a back-edge and B is not
        // strictly dominated by C
        assert!(df[&BlockId(2)].contains(&BlockId(1)));
        // B(1) is in its own DF (it's a loop header)
        assert!(df[&BlockId(1)].contains(&BlockId(1)));
    }

    #[test]
    fn test_unreachable_block() {
        // A(0) → B(1) → return, C(2) is unreachable
        let func = make_function(vec![
            make_block(0, Terminator::Jump { target: BlockId(1) }),
            make_block(1, Terminator::Return { value: None }),
            make_block(2, Terminator::Return { value: None }), // dead
        ]);

        let tree = DominatorTree::build(&func);

        assert_eq!(tree.idom(BlockId(0)), None);
        assert_eq!(tree.idom(BlockId(1)), Some(BlockId(0)));
        // BlockId(2) not in tree at all
        assert!(!tree.dominates(BlockId(0), BlockId(2)));
    }

    #[test]
    fn test_children() {
        // A(0) → B(1), A(0) → C(2)
        let func = make_function(vec![
            make_block(
                0,
                Terminator::If {
                    condition: crate::ir::VarId(0),
                    then_target: BlockId(1),
                    else_target: BlockId(2),
                    span: ast::Span::default(),
                },
            ),
            make_block(1, Terminator::Return { value: None }),
            make_block(2, Terminator::Return { value: None }),
        ]);

        let tree = DominatorTree::build(&func);
        let mut children = tree.children(BlockId(0)).to_vec();
        children.sort_by_key(|b| b.0);
        assert_eq!(children, vec![BlockId(1), BlockId(2)]);
    }
}
