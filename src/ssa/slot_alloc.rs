//! Slot allocation: map SSA VarIds to physical stack-slot offsets so that
//! non-interfering variables share a slot, shrinking each function's frame.
//!
//! Built at compile time (consumed by `compile_function`), so the IR and
//! `analyze_types` keep operating on the original VarIds — only *storage* is
//! shared. This preserves per-VarId type specialization: coalesced vars have
//! disjoint live ranges, so the shared slot holds the correctly-typed value
//! whenever each is live.
//!
//! Algorithm: liveness → interference graph (vars simultaneously live conflict)
//! → greedy coloring; colors are physical slots. Two constraints layer on top:
//!
//! - **Parameters** are pre-colored to their positional slots `0..param_count`
//!   (`rest_param` next): the calling convention adopts args into those slots.
//! - **Reference safety** — `Slot::Ref`/`Accessor` store slot indices captured
//!   at creation and dereferenced directly by the VM, so a slot reused while a
//!   reference into it is live would corrupt. v1 conservatively *pins* every var
//!   that is a dest/base/key of a `MakeRef`/`MakeAccessor` to its own slot.
//! - **Tail calls** rewrite the current frame in place; the param region must
//!   not be shared with body temps (see `compile_terminator`'s TailCall reset),
//!   so when a function contains a `TailCall` the param slots are exclusive.
//!
//! Move/copy coalescing is deferred (v2): it must be type-aware (narrowing
//! copies carry a tighter type), and greedy coloring already reclaims the bulk.

use crate::ir::{BasicBlock, BlockId, Function, Instruction, Terminator, VarId, uses};
use crate::ssa::liveness::Liveness;
use std::collections::{BTreeSet, HashMap, HashSet};

/// A physical-slot assignment for one function's SSA variables.
pub struct SlotAlloc {
    /// Physical slot per VarId, indexed by `var.0` (dense over `0..locals.len()`).
    colors: Vec<u32>,
    frame_size: usize,
}

impl SlotAlloc {
    /// Compute a slot assignment for `function`. Takes a pre-built block map
    /// (`cfg::block_map(function)`) like `Liveness::build`/`DominatorTree::build`.
    pub fn build(function: &Function, block_map: &HashMap<BlockId, &BasicBlock>) -> Self {
        let n = function.locals.len();
        if n == 0 {
            return SlotAlloc {
                colors: Vec::new(),
                frame_size: 0,
            };
        }

        let liveness = Liveness::build(function, block_map);

        // ── Interference graph ────────────────────────────────────────────
        // Edge (a,b) iff a and b are ever simultaneously live. Built at each
        // def against the set live immediately after it (standard).
        let mut adj: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); n];
        let add_edge = |adj: &mut Vec<BTreeSet<u32>>, a: VarId, b: VarId| {
            if a != b {
                adj[a.0 as usize].insert(b.0);
                adj[b.0 as usize].insert(a.0);
            }
        };

        for block in &function.blocks {
            let mut live: BTreeSet<VarId> = liveness.live_out(block.id).clone();
            for r in uses::terminator_reads(&block.terminator) {
                live.insert(r);
            }
            // Walk instructions backward. Phi nodes are skipped: their dests are
            // pinned below (so their interference is handled by pinning), and
            // their operands are credited to predecessor live_out by Liveness.
            for inst in block.instructions.iter().rev() {
                if matches!(inst.node, Instruction::Phi { .. }) {
                    continue;
                }
                if let Some(d) = uses::instruction_dest(&inst.node) {
                    for &v in &live {
                        add_edge(&mut adj, d, v);
                    }
                    live.remove(&d);
                }
                for r in uses::instruction_reads(&inst.node) {
                    live.insert(r);
                }
            }
        }

        // ── Pinning ───────────────────────────────────────────────────────
        // A pinned var keeps a private slot for the whole function (it interferes
        // with every other var). Two reasons to pin:
        //
        // 1. Ref/Accessor operands — `Slot::Ref`/`Accessor` store slot indices
        //    captured at creation and dereferenced directly, so the slots a live
        //    reference points into must not be reused.
        // 2. Phi dests — phi-resolution copies are inserted at predecessor block
        //    ends and run on *all* of a predecessor's out-edges, so on a critical
        //    edge a phi dest is physically live beyond what SSA liveness reports
        //    (a `break`/loop value preloaded at a header survives along the body
        //    path). Pinning keeps that slot private so the preloaded value can't
        //    be clobbered. (A v2 with critical-edge splitting could coalesce phi
        //    dests; v1 keeps it simple and correct.)
        let mut pinned: HashSet<u32> = HashSet::new();
        for block in &function.blocks {
            for inst in &block.instructions {
                match &inst.node {
                    Instruction::Phi { dest, .. } => {
                        pinned.insert(dest.0);
                    }
                    Instruction::MakeRef { dest, base } => {
                        pinned.insert(dest.0);
                        pinned.insert(base.0);
                    }
                    Instruction::MakeAccessor { dest, base, key } => {
                        pinned.insert(dest.0);
                        pinned.insert(base.0);
                        pinned.insert(key.0);
                    }
                    _ => {}
                }
            }
        }
        for &p in &pinned {
            for v in 0..n as u32 {
                add_edge(&mut adj, VarId(p), VarId(v));
            }
        }

        // ── Coloring ──────────────────────────────────────────────────────
        let param_count = function.params.len();
        let mut color: Vec<Option<u32>> = vec![None; n];
        for (i, p) in function.params.iter().enumerate() {
            color[p.0 as usize] = Some(i as u32);
        }
        let mut param_region = param_count as u32;
        if let Some(r) = function.rest_param {
            color[r.0 as usize] = Some(param_region);
            param_region += 1;
        }

        // Tail calls rewrite the frame in place; body temps must not occupy the
        // param region (the TailCall reset preserves `0..param_count`).
        let has_tail_call = function
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, Terminator::TailCall { .. }));
        let min_color = if has_tail_call { param_region } else { 0 };

        for v in 0..n {
            if color[v].is_some() {
                continue;
            }
            let mut neighbor_colors: BTreeSet<u32> = BTreeSet::new();
            for &nb in &adj[v] {
                if let Some(c) = color[nb as usize] {
                    neighbor_colors.insert(c);
                }
            }
            let mut c = min_color;
            while neighbor_colors.contains(&c) {
                c += 1;
            }
            color[v] = Some(c);
        }

        let colors: Vec<u32> = color.into_iter().map(|c| c.unwrap_or(0)).collect();
        let frame_size = colors
            .iter()
            .copied()
            .max()
            .map(|m| m as usize + 1)
            .unwrap_or(0)
            .max(param_region as usize);

        SlotAlloc { colors, frame_size }
    }

    /// Physical slot for `var`.
    pub fn slot(&self, var: VarId) -> usize {
        self.colors[var.0 as usize] as usize
    }

    /// Number of physical slots the function's frame needs.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{IntrinsicOp, Literal, cfg};

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

    /// A function with `n` locals (VarId 0..n), given params and blocks.
    fn func(n: u32, params: Vec<VarId>, blocks: Vec<BasicBlock>) -> Function {
        use crate::types::TypeSet;
        let locals = (0..n)
            .map(|i| {
                crate::ir::Var::new(VarId(i), ast::Identifier(format!("v{i}")), TypeSet::any())
            })
            .collect();
        Function {
            name: ast::Identifier("test".to_string()),
            params,
            rest_param: None,
            locals,
            blocks,
            entry_block: BlockId(0),
        }
    }

    fn alloc(f: &Function) -> SlotAlloc {
        SlotAlloc::build(f, &cfg::block_map(f))
    }

    fn konst(dest: u32, v: u64) -> Instruction {
        Instruction::Const {
            dest: VarId(dest),
            value: Literal::UInt(v),
        }
    }

    #[test]
    fn test_disjoint_temps_share_slot() {
        // v0=1; use v0 (ret-ish via add); v1=2; ret v1+? — keep simple:
        // b0: v0 = 0; v1 = v0 + v0 (v0 dies); v2 = 1 (v0 dead, can reuse); ret v2
        let f = func(
            3,
            vec![],
            vec![block(
                0,
                vec![
                    konst(0, 0),
                    Instruction::Intrinsic {
                        dest: VarId(1),
                        op: IntrinsicOp::Add,
                        args: vec![VarId(0), VarId(0)],
                    },
                    konst(2, 1),
                ],
                Terminator::Return {
                    value: Some(VarId(2)),
                },
            )],
        );
        let a = alloc(&f);
        // 3 SSA vars but disjoint lifetimes → fewer than 3 slots.
        assert!(a.frame_size() < 3, "frame_size={}", a.frame_size());
    }

    #[test]
    fn test_overlapping_temps_distinct() {
        // v0=1; v1=2; v2 = v0 + v1 (both live across) → v0,v1 distinct slots
        let f = func(
            3,
            vec![],
            vec![block(
                0,
                vec![
                    konst(0, 1),
                    konst(1, 2),
                    Instruction::Intrinsic {
                        dest: VarId(2),
                        op: IntrinsicOp::Add,
                        args: vec![VarId(0), VarId(1)],
                    },
                ],
                Terminator::Return {
                    value: Some(VarId(2)),
                },
            )],
        );
        let a = alloc(&f);
        assert_ne!(a.slot(VarId(0)), a.slot(VarId(1)));
    }

    #[test]
    fn test_params_precolored() {
        // fn f(p0, p1) { p0 + p1 }
        let f = func(
            3,
            vec![VarId(0), VarId(1)],
            vec![block(
                0,
                vec![Instruction::Intrinsic {
                    dest: VarId(2),
                    op: IntrinsicOp::Add,
                    args: vec![VarId(0), VarId(1)],
                }],
                Terminator::Return {
                    value: Some(VarId(2)),
                },
            )],
        );
        let a = alloc(&f);
        assert_eq!(a.slot(VarId(0)), 0);
        assert_eq!(a.slot(VarId(1)), 1);
        assert!(a.frame_size() >= 2);
    }

    #[test]
    fn test_accessor_pinning_unique() {
        // v0 = [..] (collection), v1 = key, v2 = MakeAccessor(v0, v1), many temps,
        // then use v2. Pinned: v0, v1, v2 — all distinct slots.
        let f = func(
            5,
            vec![],
            vec![block(
                0,
                vec![
                    konst(0, 0),
                    konst(1, 0),
                    Instruction::MakeAccessor {
                        dest: VarId(2),
                        base: VarId(0),
                        key: VarId(1),
                    },
                    konst(3, 9),
                    Instruction::WriteRef {
                        ref_var: VarId(2),
                        value: VarId(3),
                    },
                ],
                Terminator::Return { value: None },
            )],
        );
        let a = alloc(&f);
        let (s0, s1, s2) = (a.slot(VarId(0)), a.slot(VarId(1)), a.slot(VarId(2)));
        assert_ne!(s0, s1);
        assert_ne!(s0, s2);
        assert_ne!(s1, s2);
        // v3 (a plain temp) must not reuse any pinned slot.
        let s3 = a.slot(VarId(3));
        assert!(s3 != s0 && s3 != s1 && s3 != s2);
    }

    #[test]
    fn test_deterministic() {
        let mk = || {
            func(
                3,
                vec![VarId(0)],
                vec![block(
                    0,
                    vec![
                        konst(1, 5),
                        Instruction::Intrinsic {
                            dest: VarId(2),
                            op: IntrinsicOp::Add,
                            args: vec![VarId(0), VarId(1)],
                        },
                    ],
                    Terminator::Return {
                        value: Some(VarId(2)),
                    },
                )],
            )
        };
        let a = alloc(&mk());
        let b = alloc(&mk());
        assert_eq!(a.colors, b.colors);
        assert_eq!(a.frame_size, b.frame_size);
    }

    #[test]
    fn test_empty_function() {
        let f = func(0, vec![], vec![block(0, vec![], Terminator::Return { value: None })]);
        let a = alloc(&f);
        assert_eq!(a.frame_size(), 0);
    }

    #[test]
    fn test_tailcall_param_region_exclusive() {
        // fn f(p0) { ... tailcall(t) } — a body temp must not occupy slot 0.
        let f = func(
            2,
            vec![VarId(0)],
            vec![block(
                0,
                vec![konst(1, 7)],
                Terminator::TailCall {
                    args: vec![VarId(1)],
                },
            )],
        );
        let a = alloc(&f);
        assert_eq!(a.slot(VarId(0)), 0); // param
        assert!(a.slot(VarId(1)) >= 1); // body temp above the param region
    }
}
