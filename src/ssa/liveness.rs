//! Liveness analysis over SSA: which SSA variables are live on entry to and
//! exit from each block.
//!
//! Standard backward dataflow (Appel) with the SSA phi rule: a phi defines its
//! dest at the top of its block, and each phi operand is a use at the *end of
//! the corresponding predecessor* (not at the phi's own block). The fixpoint is
//! the unique least solution, so the result is independent of iteration order;
//! sets are stored as `BTreeSet` for deterministic iteration.
//!
//! Mirrors `DominatorTree`'s shape (`build(function, block_map)`). `live_in`/
//! `live_out` are the live ranges the upcoming slot allocator will consume;
//! `used` is the global "read anywhere" set. The module currently has no
//! non-test consumer (the slot allocator lands next), so it is gated like
//! domtree's reserved accessors and exercised by its unit tests.
#![cfg_attr(not(test), allow(dead_code))]

use crate::ir::{BasicBlock, BlockId, Function, Instruction, VarId, cfg, uses};
use std::collections::{BTreeSet, HashMap, HashSet};

/// Per-block live-in / live-out sets over a function's SSA variables.
pub struct Liveness {
    live_in: HashMap<BlockId, BTreeSet<VarId>>,
    live_out: HashMap<BlockId, BTreeSet<VarId>>,
    /// Every VarId read somewhere in a reachable block.
    used: BTreeSet<VarId>,
    /// Returned for blocks absent from the maps (unreachable).
    empty: BTreeSet<VarId>,
}

impl Liveness {
    /// Compute liveness for `function`. Takes a pre-built block map
    /// (`cfg::block_map(function)`) so a caller that already has one does not
    /// rebuild it — same convention as `DominatorTree::build`.
    pub fn build(function: &Function, block_map: &HashMap<BlockId, &BasicBlock>) -> Self {
        let reachable = cfg::reachable_blocks(function, block_map);

        // Deterministic block order. The fixpoint solution is order-independent;
        // a fixed order just keeps iteration counts stable.
        let mut order: Vec<BlockId> = reachable.iter().copied().collect();
        order.sort_unstable_by_key(|b| b.0);

        // Per-block precompute.
        let mut defs: HashMap<BlockId, HashSet<VarId>> = HashMap::new();
        let mut phi_defs: HashMap<BlockId, HashSet<VarId>> = HashMap::new();
        let mut gen_map: HashMap<BlockId, BTreeSet<VarId>> = HashMap::new();
        // Phi operands grouped by (phi's block) → (predecessor) → operand vars.
        let mut phi_uses: HashMap<BlockId, HashMap<BlockId, Vec<VarId>>> = HashMap::new();
        let mut used: BTreeSet<VarId> = BTreeSet::new();

        for &b in &order {
            let block = block_map[&b];
            let mut block_defs: HashSet<VarId> = HashSet::new();
            let mut block_phi_defs: HashSet<VarId> = HashSet::new();
            let mut upward: BTreeSet<VarId> = BTreeSet::new();
            // Defined-so-far within b, seeded with phi dests (defined at the top
            // of the block) so a later read of a phi result is not upward-exposed.
            let mut defined: HashSet<VarId> = HashSet::new();

            // Phis: dest is a (phi) def; operands are uses at their predecessor.
            for inst in &block.instructions {
                if let Instruction::Phi { dest, sources } = &inst.node {
                    block_defs.insert(*dest);
                    block_phi_defs.insert(*dest);
                    defined.insert(*dest);
                    for (pred, v) in sources {
                        phi_uses
                            .entry(b)
                            .or_default()
                            .entry(*pred)
                            .or_default()
                            .push(*v);
                        used.insert(*v);
                    }
                }
            }

            // Non-phi instructions in order: a read is upward-exposed unless
            // already defined earlier in b; each dest becomes defined.
            for inst in &block.instructions {
                if matches!(inst.node, Instruction::Phi { .. }) {
                    continue;
                }
                for r in uses::instruction_reads(&inst.node) {
                    used.insert(r);
                    if !defined.contains(&r) {
                        upward.insert(r);
                    }
                }
                if let Some(d) = uses::instruction_dest(&inst.node) {
                    block_defs.insert(d);
                    defined.insert(d);
                }
            }
            for r in uses::terminator_reads(&block.terminator) {
                used.insert(r);
                if !defined.contains(&r) {
                    upward.insert(r);
                }
            }

            defs.insert(b, block_defs);
            phi_defs.insert(b, block_phi_defs);
            gen_map.insert(b, upward);
        }

        // Backward fixpoint:
        //   live_out[b] = ⋃ over succ s of ( (live_in[s] \ phi_defs[s]) ∪ phi_uses[s][b] )
        //   live_in[b]  = gen[b] ∪ ( live_out[b] \ defs[b] )
        let mut live_in: HashMap<BlockId, BTreeSet<VarId>> =
            order.iter().map(|&b| (b, BTreeSet::new())).collect();
        let mut live_out: HashMap<BlockId, BTreeSet<VarId>> =
            order.iter().map(|&b| (b, BTreeSet::new())).collect();

        let mut changed = true;
        while changed {
            changed = false;
            for &b in order.iter().rev() {
                let mut out: BTreeSet<VarId> = BTreeSet::new();
                for s in block_map[&b].terminator.successors() {
                    if !reachable.contains(&s) {
                        continue;
                    }
                    let s_phi_defs = &phi_defs[&s];
                    for v in &live_in[&s] {
                        if !s_phi_defs.contains(v) {
                            out.insert(*v);
                        }
                    }
                    if let Some(by_pred) = phi_uses.get(&s)
                        && let Some(operands) = by_pred.get(&b)
                    {
                        out.extend(operands.iter().copied());
                    }
                }

                let mut in_: BTreeSet<VarId> = gen_map[&b].clone();
                let b_defs = &defs[&b];
                for v in &out {
                    if !b_defs.contains(v) {
                        in_.insert(*v);
                    }
                }

                if out != live_out[&b] {
                    live_out.insert(b, out);
                    changed = true;
                }
                if in_ != live_in[&b] {
                    live_in.insert(b, in_);
                    changed = true;
                }
            }
        }

        Liveness {
            live_in,
            live_out,
            used,
            empty: BTreeSet::new(),
        }
    }

    /// Variables live on entry to `block`.
    pub fn live_in(&self, block: BlockId) -> &BTreeSet<VarId> {
        self.live_in.get(&block).unwrap_or(&self.empty)
    }

    /// Variables live on exit from `block`.
    pub fn live_out(&self, block: BlockId) -> &BTreeSet<VarId> {
        self.live_out.get(&block).unwrap_or(&self.empty)
    }

    /// Every VarId read somewhere in a reachable block.
    pub fn used(&self) -> &BTreeSet<VarId> {
        &self.used
    }

    /// Whether `var` is read anywhere in a reachable block.
    pub fn is_used(&self, var: VarId) -> bool {
        self.used.contains(&var)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{IntrinsicOp, Literal, Terminator};

    fn si(inst: Instruction) -> ast::Spanned<Instruction> {
        ast::Spanned::new(inst, ast::Span::default())
    }

    fn block(id: u32, instructions: Vec<Instruction>, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            instructions: instructions.into_iter().map(si).collect(),
            terminator,
        }
    }

    fn func(blocks: Vec<BasicBlock>) -> Function {
        Function {
            blocks,
            entry_block: BlockId(0),
            ..Default::default()
        }
    }

    fn live(f: &Function) -> Liveness {
        Liveness::build(f, &cfg::block_map(f))
    }

    fn set(ids: &[u32]) -> BTreeSet<VarId> {
        ids.iter().map(|&i| VarId(i)).collect()
    }

    fn konst(dest: u32, v: u64) -> Instruction {
        Instruction::Const {
            dest: VarId(dest),
            value: Literal::UInt(v),
        }
    }

    #[test]
    fn test_linear_chain() {
        // B0: v0 = 0; dead = 1; -> B1 ; B1: -> B2 ; B2: return v0
        let f = func(vec![
            block(
                0,
                vec![konst(0, 0), konst(9, 1)],
                Terminator::Jump { target: BlockId(1) },
            ),
            block(1, vec![], Terminator::Jump { target: BlockId(2) }),
            block(
                2,
                vec![],
                Terminator::Return {
                    value: Some(VarId(0)),
                },
            ),
        ]);
        let l = live(&f);

        assert_eq!(l.live_in(BlockId(0)), &set(&[])); // v0 defined here
        assert_eq!(l.live_out(BlockId(0)), &set(&[0]));
        assert_eq!(l.live_in(BlockId(1)), &set(&[0]));
        assert_eq!(l.live_in(BlockId(2)), &set(&[0]));
        assert_eq!(l.live_out(BlockId(2)), &set(&[]));

        assert!(l.is_used(VarId(0)));
        assert!(!l.is_used(VarId(9))); // pure unused temp
        assert_eq!(l.used(), &set(&[0]));
    }

    #[test]
    fn test_diamond_phi() {
        // B0: c=1; if c -> B1,B2 ; B1: x1=1 -> B3 ; B2: x2=2 -> B3
        // B3: x3 = phi[(B1,x1),(B2,x2)]; return x3
        let f = func(vec![
            block(
                0,
                vec![konst(10, 1)],
                Terminator::If {
                    condition: VarId(10),
                    then_target: BlockId(1),
                    else_target: BlockId(2),
                    span: ast::Span::default(),
                },
            ),
            block(1, vec![konst(1, 1)], Terminator::Jump { target: BlockId(3) }),
            block(2, vec![konst(2, 2)], Terminator::Jump { target: BlockId(3) }),
            block(
                3,
                vec![Instruction::Phi {
                    dest: VarId(3),
                    sources: vec![(BlockId(1), VarId(1)), (BlockId(2), VarId(2))],
                }],
                Terminator::Return {
                    value: Some(VarId(3)),
                },
            ),
        ]);
        let l = live(&f);

        // Each phi operand is live out of its own predecessor only.
        assert_eq!(l.live_out(BlockId(1)), &set(&[1]));
        assert_eq!(l.live_out(BlockId(2)), &set(&[2]));
        // The phi dest is defined at B3; nothing is live into B3.
        assert_eq!(l.live_in(BlockId(3)), &set(&[]));
    }

    #[test]
    fn test_loop_back_edge() {
        // B0: i0=0 -> B1
        // B1: i = phi[(B0,i0),(B2,i2)]; if i -> B2,B3
        // B2: i2 = i + i -> B1    (i2 never read outside the loop)
        // B3: return
        let f = func(vec![
            block(0, vec![konst(0, 0)], Terminator::Jump { target: BlockId(1) }),
            block(
                1,
                vec![Instruction::Phi {
                    dest: VarId(1),
                    sources: vec![(BlockId(0), VarId(0)), (BlockId(2), VarId(2))],
                }],
                Terminator::If {
                    condition: VarId(1),
                    then_target: BlockId(2),
                    else_target: BlockId(3),
                    span: ast::Span::default(),
                },
            ),
            block(
                2,
                vec![Instruction::Intrinsic {
                    dest: VarId(2),
                    op: IntrinsicOp::Add,
                    args: vec![VarId(1), VarId(1)],
                }],
                Terminator::Jump { target: BlockId(1) },
            ),
            block(3, vec![], Terminator::Return { value: None }),
        ]);
        let l = live(&f);

        // Loop-carried liveness: the init flows in, and the back-edge operand i2
        // stays live out of the latch even though i2 is never read outside the
        // loop — standard liveness keeps a dead accumulator alive (DCE removes
        // such cycles separately).
        assert!(l.live_out(BlockId(0)).contains(&VarId(0)));
        assert!(l.live_out(BlockId(2)).contains(&VarId(2)));
    }

    #[test]
    fn test_unreachable_block() {
        // B0: return ; B1 (unreachable): w = v + v; return w
        let f = func(vec![
            block(0, vec![], Terminator::Return { value: None }),
            block(
                1,
                vec![Instruction::Intrinsic {
                    dest: VarId(8),
                    op: IntrinsicOp::Add,
                    args: vec![VarId(7), VarId(7)],
                }],
                Terminator::Return {
                    value: Some(VarId(8)),
                },
            ),
        ]);
        let l = live(&f);

        // Vars used only in unreachable code are invisible.
        assert!(!l.is_used(VarId(7)));
        assert!(!l.is_used(VarId(8)));
        assert_eq!(l.live_in(BlockId(1)), &set(&[]));
    }

    #[test]
    fn test_deterministic() {
        let mk = || {
            func(vec![
                block(
                    0,
                    vec![konst(10, 1)],
                    Terminator::If {
                        condition: VarId(10),
                        then_target: BlockId(1),
                        else_target: BlockId(2),
                        span: ast::Span::default(),
                    },
                ),
                block(1, vec![konst(1, 1)], Terminator::Jump { target: BlockId(3) }),
                block(2, vec![konst(2, 2)], Terminator::Jump { target: BlockId(3) }),
                block(
                    3,
                    vec![Instruction::Phi {
                        dest: VarId(3),
                        sources: vec![(BlockId(1), VarId(1)), (BlockId(2), VarId(2))],
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ])
        };
        let a = live(&mk());
        let b = live(&mk());
        assert_eq!(a.live_in, b.live_in);
        assert_eq!(a.live_out, b.live_out);
    }
}
