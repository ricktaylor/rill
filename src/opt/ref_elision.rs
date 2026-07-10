//! Reference Elision Optimization
//!
//! Simplifies MakeRef instructions when the full reference indirection is
//! unnecessary. Three rewrites:
//!
//! 1. **Ref→Accessor flattening** — `MakeRef(dest, base)` where `base` is from
//!    `MakeAccessor(_, arr, key)`: rewrite to `MakeAccessor(dest, arr, key)`.
//!    Eliminates Ref(Accessor) double indirection.
//!
//! 2. **Ref chain shortening** — `MakeRef(dest, base)` where `base` is itself
//!    from `MakeRef(_, original)`: rewrite to `MakeRef(dest, original)`.
//!    Eliminates multi-hop `Slot::Ref` chains.
//!
//! 3. **Read-only ref demotion** — `MakeRef(dest, base)` where no `WriteRef`
//!    targets `dest`, base not in `written_bases`, and `dest` does not escape
//!    to a callee: demote to `Copy(dest, base)`. Eliminates `Slot::Ref`.
//!
//! 4. **Read-only Accessor demotion** — `MakeAccessor(dest, base, key)` where
//!    no `WriteRef` targets `dest`, the resolved base is not in
//!    `written_bases`, and `dest` does not escape to a callee: demote to
//!    `Index(dest, base, key)`. A read-only Accessor over an unwritten base
//!    is just an expensive Index (double dereference on every read). An
//!    accessor over a WRITTEN base must stay live so reads through the alias
//!    observe the mutation, and an escaped accessor may be written by the
//!    callee through the copied slot.
//!
//! Safe to run repeatedly in the optimizer fixpoint loop.

use crate::ir::{Function, Instruction, VarId};
use std::collections::{HashMap, HashSet};

/// Metadata for a MakeRef instruction.
struct RefInfo {
    base: VarId,
    key: Option<VarId>,
}

/// Follow MakeRef chains to find the ultimate base.
///
/// For `MakeRef(v2, v1)` where `v1 = MakeRef(_, v0)`, returns `v0`.
/// Stops at MakeAccessor origins (key: Some) or non-ref VarIds.
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
    let mut write_accessor_bases: HashSet<VarId> = HashSet::new();
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
                Instruction::WriteAccessor { base, .. } => {
                    // WriteAccessor targets the base directly — mark it as written
                    write_accessor_bases.insert(*base);
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
    // A base is "written" if any WriteRef or WriteAccessor modifies it.
    // Refs aliasing a written base must stay live so reads see the mutation.

    let mut written_bases: HashSet<VarId> = HashSet::new();
    // WriteRef: trace through the ref to find the ultimate base
    for ref_var in &write_ref_targets {
        if let Some(info) = make_refs.get(ref_var) {
            let resolved = resolve_base(info.base, &make_refs);
            written_bases.insert(resolved);
        }
    }
    // WriteAccessor: base is written directly
    for base in &write_accessor_bases {
        let resolved = resolve_base(*base, &make_refs);
        written_bases.insert(resolved);
    }

    // ── Phase 2b: Compute escaped refs ───────────────────────────────────
    //
    // A ref/accessor passed as a Call argument escapes: the callee receives
    // the slot as-is and may write through it. So does any ref/accessor a
    // call arg was built FROM (the call site wraps the original binding in
    // a MakeRef), transitively through ref chains — demoting the underlying
    // binding would leave post-call reads on a stale snapshot.
    let mut escaped: HashSet<VarId> = call_arg_refs;
    loop {
        let mut grew = false;
        for (dest, info) in &make_refs {
            if escaped.contains(dest)
                && make_refs.contains_key(&info.base)
                && escaped.insert(info.base)
            {
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    // ── Phase 3: Rewrite ─────────────────────────────────────────────────

    let mut rewrites = 0;

    for block in &mut function.blocks {
        for inst in &mut block.instructions {
            match &inst.node {
                Instruction::MakeRef { dest, base } => {
                    let dest = *dest;
                    let base = *base;

                    // Ref(Accessor) → Accessor: flatten double indirection
                    if let Some(RefInfo {
                        base: acc_base,
                        key: Some(acc_key),
                    }) = make_refs.get(&base)
                    {
                        inst.node = Instruction::MakeAccessor {
                            dest,
                            base: *acc_base,
                            key: *acc_key,
                        };
                        rewrites += 1;
                        continue;
                    }

                    let resolved = resolve_base(base, &make_refs);

                    if !write_ref_targets.contains(&dest)
                        && !written_bases.contains(&resolved)
                        && !escaped.contains(&dest)
                    {
                        // Read-only ref → demote to Copy
                        inst.node = Instruction::Copy {
                            dest,
                            src: resolved,
                        };
                        rewrites += 1;
                    } else if resolved != base {
                        // Shorten ref chain
                        inst.node = Instruction::MakeRef {
                            dest,
                            base: resolved,
                        };
                        rewrites += 1;
                    }
                }

                Instruction::MakeAccessor { dest, base, key } => {
                    let dest = *dest;
                    let base = *base;
                    let key = *key;

                    let resolved = resolve_base(base, &make_refs);

                    // Read-only Accessor over an unwritten, non-escaping base
                    // → demote to Index (plain read, no Slot::Accessor
                    // overhead). If the base is written through ANY path the
                    // accessor must stay live so reads observe the mutation.
                    if !write_ref_targets.contains(&dest)
                        && !written_bases.contains(&resolved)
                        && !escaped.contains(&dest)
                    {
                        inst.node = Instruction::Index { dest, base, key };
                        rewrites += 1;
                    }
                }

                _ => continue,
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
    fn test_read_only_accessor_demoted_to_index() {
        // MakeAccessor(v2, v0, v1) with no WriteRef/WriteAccessor → becomes Index
        // A read-only Accessor is just an expensive Index.
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
    fn test_accessor_kept_when_sibling_writes_same_base() {
        // v1 = MakeAccessor(v0, v_idx)  — read-only, but v0 is written via v2
        // v2 = MakeAccessor(v0, v_idx2) — has WriteRef → stays MakeAccessor
        // BOTH must stay live: reads through v1 must observe v2's mutation.
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

        // v0 is in written_bases (v2's WriteRef), so neither accessor demotes
        assert_eq!(rewrites, 0);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::MakeAccessor { .. }
        ));
        assert!(matches!(
            &func.blocks[0].instructions[3].node,
            Instruction::MakeAccessor { .. }
        ));
    }

    #[test]
    fn test_accessor_kept_when_base_written_via_write_accessor() {
        // v2 = MakeAccessor(v0, v1); WriteAccessor{base: v0} elsewhere
        // → v2 must stay live so reads through it see the element write
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
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteAccessor {
                    base: var(0),
                    key: var(1),
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
    fn test_accessor_kept_when_it_escapes_via_call_arg_ref() {
        // v2 = MakeAccessor(v0, v1); v3 = MakeRef(v2); Call(_, [v3])
        // The callee may write through v3's copied slot, so both v3 (call
        // arg) and v2 (what it aliases) must stay live. v3 is flattened to
        // a direct Accessor (rewrite #1); v2 must NOT demote to Index.
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
                si(Instruction::MakeRef {
                    dest: var(3),
                    base: var(2),
                }),
                si(Instruction::Call {
                    dest: var(4),
                    function: crate::ir::FunctionRef {
                        namespace: None,
                        name: ast::Identifier("f".into()),
                    },
                    args: vec![var(3)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks);
        let rewrites = elide_refs(&mut func);

        // Only the Ref(Accessor) flatten fires; no demotions
        assert_eq!(rewrites, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::MakeAccessor { .. }
        ));
        assert!(matches!(
            &func.blocks[0].instructions[3].node,
            Instruction::MakeAccessor { dest, base, key }
                if *dest == var(3) && *base == var(0) && *key == var(1)
        ));
    }
}
