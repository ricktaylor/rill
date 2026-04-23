//! Reference Elision Optimization
//!
//! Simplifies MakeRef and MakeAccessor instructions when the full reference
//! indirection is unnecessary. Three rewrites, in order of application:
//!
//! 1. **Ref chain shortening** — `MakeRef(dest, base)` where `base` is itself
//!    from `MakeRef(_, original)`: rewrite to `MakeRef(dest, original)`.
//!    Eliminates multi-hop `Slot::Ref` chains at runtime.
//!
//! 2. **Read-only element ref demotion** — `MakeAccessor(dest, base, k)` where no
//!    `WriteRef` targets `dest`: demote to `Index(dest, base, k)`. The ref metadata
//!    was only needed for write-back; without it, a plain read suffices.
//!
//! 3. **Read-only whole-value ref demotion** — `MakeRef(dest, base)` where no
//!    `WriteRef` in the function modifies `base` (directly or transitively): demote
//!    to `Copy(dest, base)`. Eliminates the `Slot::Ref` indirection entirely.
//!
//! Safe to run repeatedly in the optimizer fixpoint loop — each pass may expose
//! new opportunities as other passes (const fold, DCE, CFG simplify) remove WriteRefs
//! or MakeRefs.

use crate::ir::{Function, Instruction, VarId};
use std::collections::{HashMap, HashSet};

/// Metadata for a MakeRef instruction.
struct RefInfo {
    base: VarId,
    key: Option<VarId>,
}

/// Follow whole-value MakeRef chains to find the ultimate base.
///
/// For `MakeRef(v2, v1, None)` where `v1 = MakeRef(_, v0, None)`, returns `v0`.
/// Stops at element refs (`key: Some`) or non-MakeRef origins.
/// Bounded iteration prevents infinite loops on malformed IR.
fn resolve_base(var: VarId, make_refs: &HashMap<VarId, RefInfo>) -> VarId {
    let mut current = var;
    for _ in 0..64 {
        match make_refs.get(&current) {
            Some(RefInfo { base, key: None }) => current = *base,
            _ => break,
        }
    }
    current
}

/// Elide unnecessary MakeRef instructions.
///
/// Returns the number of instructions rewritten (for fixpoint convergence check).
pub fn elide_refs(function: &mut Function) -> usize {
    // ── Phase 1: Collect metadata ────────────────────────────────────────

    let mut make_refs: HashMap<VarId, RefInfo> = HashMap::new();
    let mut write_ref_targets: HashSet<VarId> = HashSet::new();
    let mut call_arg_refs: HashSet<VarId> = HashSet::new();

    for block in &function.blocks {
        for inst in &block.instructions {
            match &inst.node {
                Instruction::MakeAccessor { dest, base, key } => {
                    make_refs.insert(
                        *dest,
                        RefInfo {
                            base: *base,
                            key: Some(*key),
                        },
                    );
                }
                Instruction::MakeRef { dest, base } => {
                    make_refs.insert(
                        *dest,
                        RefInfo {
                            base: *base,
                            key: None,
                        },
                    );
                }
                Instruction::WriteRef { ref_var, .. } => {
                    write_ref_targets.insert(*ref_var);
                }
                // MakeRef dests passed as Call args must not be demoted —
                // the callee may write through the Slot::Ref.
                Instruction::Call { args, .. } => {
                    for arg in args {
                        call_arg_refs.insert(*arg);
                    }
                }
                _ => {}
            }
        }
    }

    if make_refs.is_empty() {
        return 0;
    }

    // ── Phase 2: Compute written bases ───────────────────────────────────
    //
    // A base is "written" if any WriteRef in the function modifies it —
    // either as a whole-value write (key: None) or an element write
    // (key: Some, which mutates the collection at that base). In both
    // cases, a Slot::Ref alias to that base must stay live so reads
    // through the ref see the mutation.

    let mut written_bases: HashSet<VarId> = HashSet::new();
    for ref_var in &write_ref_targets {
        if let Some(info) = make_refs.get(ref_var) {
            let resolved = resolve_base(info.base, &make_refs);
            written_bases.insert(resolved);
        }
    }

    // ── Phase 3: Rewrite ─────────────────────────────────────────────────

    let mut rewrites = 0;

    for block in &mut function.blocks {
        for inst in &mut block.instructions {
            // Extract dest, base, key from MakeAccessor or MakeRef
            let (dest, base, key) = match &inst.node {
                Instruction::MakeAccessor { dest, base, key } => (*dest, *base, Some(*key)),
                Instruction::MakeRef { dest, base } => (*dest, *base, None),
                _ => continue,
            };

            match key {
                None => {
                    let resolved = resolve_base(base, &make_refs);

                    if !write_ref_targets.contains(&dest)
                        && !written_bases.contains(&resolved)
                        && !call_arg_refs.contains(&dest)
                    {
                        // No writes through this ref, no writes to its base,
                        // and not passed as a call arg → Slot::Ref is unnecessary.
                        // Demote to Copy (uses the resolved base to also
                        // eliminate any chain in a single step).
                        inst.node = Instruction::Copy {
                            dest,
                            src: resolved,
                        };
                        rewrites += 1;
                    } else if resolved != base {
                        // Base IS written, but the chain can be shortened
                        // so the runtime Slot::Ref is 1 hop instead of N.
                        inst.node = Instruction::MakeRef {
                            dest,
                            base: resolved,
                        };
                        rewrites += 1;
                    }
                }

                Some(k) => {
                    if !write_ref_targets.contains(&dest) {
                        // No write-back through this element ref → the ref
                        // metadata is unused. A plain Index read suffices.
                        inst.node = Instruction::Index { dest, base, key: k };
                        rewrites += 1;
                    }
                }
            }
        }
    }

    rewrites
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Instruction, Literal, SpannedInst, Terminator, VarId};

    fn var(id: u32) -> VarId {
        VarId(id)
    }

    fn block(id: u32) -> crate::ir::BlockId {
        crate::ir::BlockId(id)
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

    #[test]
    fn test_read_only_element_ref_demoted_to_index() {
        // MakeAccessor(v2, v0, v1) with no WriteRef → becomes Index
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(0),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(1),
                }),
                si(Instruction::MakeAccessor {
                    dest: var(2),
                    base: var(0),
                    key: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        assert_eq!(rewrites, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Index { dest, base, key }
                if *dest == var(2) && *base == var(0) && *key == var(1)
        ));
    }

    #[test]
    fn test_element_ref_with_writeback_kept() {
        // MakeAccessor(v2, v0, v1) WITH WriteRef(v2, v3) → stays MakeAccessor
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(0),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(1),
                }),
                si(Instruction::MakeAccessor {
                    dest: var(2),
                    base: var(0),
                    key: var(1),
                }),
                si(Instruction::Const {
                    dest: var(3),
                    value: Literal::UInt(42),
                }),
                si(Instruction::WriteRef {
                    ref_var: var(2),
                    value: var(3),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        assert_eq!(rewrites, 0);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::MakeAccessor { .. }
        ));
    }

    #[test]
    fn test_read_only_whole_ref_demoted_to_copy() {
        // MakeRef(v1, v0, None) with no WriteRef anywhere → becomes Copy
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::MakeRef {
                    dest: var(1),
                    base: var(0),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        assert_eq!(rewrites, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Copy { dest, src }
                if *dest == var(1) && *src == var(0)
        ));
    }

    #[test]
    fn test_whole_ref_with_writeback_kept() {
        // MakeRef(v1, v0) WITH WriteRef(v1, v2) → stays MakeRef
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::MakeRef {
                    dest: var(1),
                    base: var(0),
                }),
                si(Instruction::Const {
                    dest: var(2),
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteRef {
                    ref_var: var(1),
                    value: var(2),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        assert_eq!(rewrites, 0);
    }

    #[test]
    fn test_whole_ref_kept_when_sibling_writes_base() {
        // v1 = MakeRef(v0)  — no WriteRef for v1
        // v2 = MakeRef(v0)  — has WriteRef(v2, _)
        // v1 must stay MakeRef because v0 is mutated through v2
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::MakeRef {
                    dest: var(1),
                    base: var(0),
                }),
                si(Instruction::MakeRef {
                    dest: var(2),
                    base: var(0),
                }),
                si(Instruction::Const {
                    dest: var(3),
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteRef {
                    ref_var: var(2),
                    value: var(3),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        // v1 must NOT be demoted (v0 is in written_bases due to v2's WriteRef)
        assert_eq!(rewrites, 0);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::MakeRef { dest, base }
                if *dest == var(1) && *base == var(0)
        ));
    }

    #[test]
    fn test_chain_shortening() {
        // v1 = MakeRef(v0)
        // v2 = MakeRef(v1)
        // WriteRef(v2, _) — so v0 is a written base
        // v2 should be shortened to MakeRef(v0) but NOT demoted
        // v1 should also stay (v0 is written)
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::MakeRef {
                    dest: var(1),
                    base: var(0),
                }),
                si(Instruction::MakeRef {
                    dest: var(2),
                    base: var(1),
                }),
                si(Instruction::Const {
                    dest: var(3),
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteRef {
                    ref_var: var(2),
                    value: var(3),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        // v2 chain-shortened from MakeRef(v1) → MakeRef(v0)
        assert_eq!(rewrites, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::MakeRef { dest, base }
                if *dest == var(2) && *base == var(0)
        ));
    }

    #[test]
    fn test_chain_demoted_when_no_writes() {
        // v1 = MakeRef(v0)
        // v2 = MakeRef(v1)
        // No WriteRef anywhere → both demoted to Copy
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::MakeRef {
                    dest: var(1),
                    base: var(0),
                }),
                si(Instruction::MakeRef {
                    dest: var(2),
                    base: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        // Both demoted to Copy — v2 copies from v0 (resolved through chain)
        assert_eq!(rewrites, 2);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Copy { dest, src }
                if *dest == var(1) && *src == var(0)
        ));
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Copy { dest, src }
                if *dest == var(2) && *src == var(0)
        ));
    }

    #[test]
    fn test_element_ref_kept_when_base_written_by_sibling() {
        // v1 = MakeAccessor(v0, v_idx)  — read-only element ref
        // v2 = MakeAccessor(v0, v_idx2) — has WriteRef
        // v1 can still be demoted to Index because element MakeAccessor reads
        // a copy of the element value, not a Slot::Ref.
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(10),
                    value: Literal::UInt(0),
                }),
                si(Instruction::Const {
                    dest: var(11),
                    value: Literal::UInt(1),
                }),
                si(Instruction::MakeAccessor {
                    dest: var(1),
                    base: var(0),
                    key: var(10),
                }),
                si(Instruction::MakeAccessor {
                    dest: var(2),
                    base: var(0),
                    key: var(11),
                }),
                si(Instruction::Const {
                    dest: var(3),
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteRef {
                    ref_var: var(2),
                    value: var(3),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        // v1 demoted to Index (no WriteRef targets v1 specifically)
        // v2 stays MakeRef (has WriteRef)
        assert_eq!(rewrites, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Index { dest, base, key }
                if *dest == var(1) && *base == var(0) && *key == var(10)
        ));
        assert!(matches!(
            &func.blocks[0].instructions[3].node,
            Instruction::MakeAccessor { .. }
        ));
    }
}
