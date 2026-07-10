//! Common Subexpression Elimination (CSE)
//!
//! Within each basic block, identifies instructions that compute the same
//! result (same opcode + same operands) and replaces duplicates with a
//! Copy of the first computation's result.
//!
//! Safe for pure scalar operations in SSA form: same VarId operands
//! guarantee same values. Heap reads (Index, Len, collection Eq, pure
//! calls) are NOT stable across heap writes — VarId identity cannot prove
//! two collection references don't alias (Copy chains and Slot::Accessor
//! writes reach the same storage through different VarIds) — so any heap
//! write (WriteRef, WriteAccessor, Append, impure call, sequence
//! advancement) invalidates previously-seen heap-reading keys. Sequence
//! ops (MakeSeq/ArraySeq/SeqNext/Collect) carry heap identity or advance
//! state in place and are never merged at all.
//!
//! Runs in the Phase 1 fixpoint loop. Copy propagation + DCE clean up
//! the resulting Copy + dead original instruction.

use crate::externs::ExternRegistry;
use crate::ir::{Function, FunctionRef, Instruction, IntrinsicOp, VarId};
use std::collections::{HashMap, HashSet};

/// A hashable key representing a computation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ExprKey {
    /// Intrinsic(op, args)
    Intrinsic(IntrinsicOp, Vec<VarId>),
    /// Const(literal bytes) — we hash the literal's debug repr for simplicity
    Const(String),
    /// Index(base, key)
    Index(VarId, VarId),
    /// Pure function call(qualified_name, args)
    Call(String, Vec<VarId>),
}

/// Sequence ops are never CSE candidates: MakeSeq/ArraySeq allocate a
/// sequence with mutable iteration state (merging two `0..3` ranges would
/// share one advancing cursor), and SeqNext/Collect advance that state.
fn is_seq_op(op: IntrinsicOp) -> bool {
    matches!(
        op,
        IntrinsicOp::MakeSeq
            | IntrinsicOp::ArraySeq(_)
            | IntrinsicOp::SeqNext
            | IntrinsicOp::Collect
    )
}

/// Intrinsics whose result depends on heap contents: not stable across a
/// heap write. Len/MapKeyAt read a collection; MakeArray/MakeMap capture
/// operand values that may be collections; Eq compares collections deeply.
fn reads_heap(op: IntrinsicOp) -> bool {
    matches!(
        op,
        IntrinsicOp::Len
            | IntrinsicOp::MapKeyAt
            | IntrinsicOp::MakeArray
            | IntrinsicOp::MakeMap
            | IntrinsicOp::Eq
    )
}

/// A call is pure when the extern registry or the interprocedural purity
/// analysis says so. Only pure calls are CSE candidates; impure calls
/// clobber previously-seen heap reads.
fn is_pure_call(
    func_ref: &FunctionRef,
    externs: Option<&ExternRegistry>,
    pure_functions: &HashSet<String>,
) -> bool {
    externs
        .and_then(|registry| registry.lookup(func_ref))
        .is_some_and(|def| def.meta.purity.is_pure())
        || pure_functions.contains(&func_ref.qualified_name())
}

/// Eliminate common subexpressions within each basic block.
///
/// Returns the number of instructions replaced with Copy.
pub fn eliminate_common_subexpressions(function: &mut Function) -> usize {
    eliminate_common_subexpressions_with_purity(function, None, &HashSet::new())
}

/// CSE with interprocedural purity information.
///
/// Pure extern and user function calls with the same args are also CSE'd.
pub fn eliminate_common_subexpressions_with_purity(
    function: &mut Function,
    externs: Option<&ExternRegistry>,
    pure_functions: &HashSet<String>,
) -> usize {
    let mut changes = 0;

    for block in &mut function.blocks {
        // Map from expression key → first VarId that computed it
        let mut seen: HashMap<ExprKey, VarId> = HashMap::new();

        for inst in &mut block.instructions {
            // Purity of a Call decides both clobbering and CSE eligibility —
            // compute it once.
            let call_is_pure = match &inst.node {
                Instruction::Call {
                    function: func_ref, ..
                } => Some(is_pure_call(func_ref, externs, pure_functions)),
                _ => None,
            };

            // Heap writes invalidate previously-seen heap reads (see module
            // header: VarId identity cannot prove absence of aliasing).
            let clobbers_heap = match &inst.node {
                Instruction::WriteAccessor { .. }
                | Instruction::WriteRef { .. }
                | Instruction::Append { .. } => true,
                // SeqNext/Collect advance or drain their sequence in place
                Instruction::Intrinsic { op, .. } => {
                    matches!(op, IntrinsicOp::SeqNext | IntrinsicOp::Collect)
                }
                Instruction::Call { .. } => call_is_pure == Some(false),
                _ => false,
            };
            if clobbers_heap {
                seen.retain(|k, _| match k {
                    ExprKey::Index(..) | ExprKey::Call(..) => false,
                    ExprKey::Intrinsic(op, _) => !reads_heap(*op),
                    ExprKey::Const(_) => true,
                });
            }

            let (dest, key) = match &inst.node {
                Instruction::Intrinsic { dest, op, args } => {
                    // Scalar intrinsics are pure: same VarId args → same
                    // result, and fallible ops (Add overflow, Div by zero)
                    // produce the same undefined result for the same inputs.
                    // Sequence ops carry identity/state — never merge.
                    if is_seq_op(*op) {
                        continue;
                    }
                    (*dest, ExprKey::Intrinsic(*op, args.clone()))
                }

                Instruction::Const { dest, value } => {
                    (*dest, ExprKey::Const(format!("{:?}", value)))
                }

                Instruction::Index { dest, base, key } => (*dest, ExprKey::Index(*base, *key)),

                // Pure function calls: same function + same args → same result
                Instruction::Call {
                    dest,
                    function: func_ref,
                    args,
                } => {
                    if call_is_pure != Some(true) {
                        continue;
                    }

                    (
                        *dest,
                        ExprKey::Call(func_ref.qualified_name(), args.clone()),
                    )
                }

                _ => continue,
            };

            if let Some(&first_dest) = seen.get(&key) {
                // Duplicate — replace with Copy of the first result
                inst.node = Instruction::Copy {
                    dest,
                    src: first_dest,
                };
                changes += 1;
            } else {
                // First occurrence — record it
                seen.insert(key, dest);
            }
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, BlockId, Literal, Terminator, Var};
    use crate::types::TypeSet;

    fn var(id: u32) -> VarId {
        VarId(id)
    }
    fn block(id: u32) -> BlockId {
        BlockId(id)
    }
    fn si(inst: Instruction) -> ast::Spanned<Instruction> {
        ast::Spanned::new(inst, ast::Span::default())
    }
    fn make_function(blocks: Vec<BasicBlock>, locals: Vec<Var>) -> Function {
        Function {
            blocks,
            locals,
            ..Default::default()
        }
    }

    #[test]
    fn test_cse_duplicate_intrinsic() {
        // v2 = Add(v0, v1)
        // v3 = Add(v0, v1)  ← same expr → Copy(v3, v2)
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::uint()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Eq,
                    args: vec![var(0), var(1)],
                }),
                si(Instruction::Intrinsic {
                    dest: var(3),
                    op: IntrinsicOp::Eq,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 1);
        // Second Eq should be replaced with Copy(v3, v2)
        assert!(matches!(
            &func.blocks[0].instructions[3].node,
            Instruction::Copy { dest, src } if *dest == var(3) && *src == var(2)
        ));
    }

    #[test]
    fn test_cse_duplicate_const() {
        // v0 = Const(42)
        // v1 = Const(42)  ← same const → Copy(v1, v0)
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(42),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Copy { dest, src } if *dest == var(1) && *src == var(0)
        ));
    }

    #[test]
    fn test_cse_no_duplicate() {
        // v2 = Eq(v0, v1)
        // v3 = Lt(v0, v1)  ← different op → no CSE
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::bool()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::bool()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Eq,
                    args: vec![var(0), var(1)],
                }),
                si(Instruction::Intrinsic {
                    dest: var(3),
                    op: IntrinsicOp::Lt,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 0);
    }

    #[test]
    fn test_cse_fallible_intrinsic() {
        // Add is fallible (overflow) but still pure — CSE is safe.
        // Same inputs → same overflow → same undefined result.
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::uint()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(1)],
                }),
                si(Instruction::Intrinsic {
                    dest: var(3),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 1); // Duplicate Add CSE'd
        assert!(matches!(
            &func.blocks[0].instructions[3].node,
            Instruction::Copy { dest, src } if *dest == var(3) && *src == var(2)
        ));
    }

    #[test]
    fn test_cse_index_not_merged_across_write_accessor() {
        // v2 = Index(v0, v1)
        // WriteAccessor{base: v0, key: v1, value: v4}
        // v3 = Index(v0, v1) ← same VarIds, but the element was mutated → no CSE
        let locals = vec![
            Var::new(
                var(0),
                ast::Identifier("arr".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
            Var::new(var(1), ast::Identifier("idx".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::any()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::any()),
            Var::new(var(4), ast::Identifier("val".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Index {
                    dest: var(2),
                    base: var(0),
                    key: var(1),
                }),
                si(Instruction::Const {
                    dest: var(4),
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteAccessor {
                    base: var(0),
                    key: var(1),
                    value: var(4),
                }),
                si(Instruction::Index {
                    dest: var(3),
                    base: var(0),
                    key: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 0);
        assert!(matches!(
            &func.blocks[0].instructions[3].node,
            Instruction::Index { .. }
        ));
    }

    #[test]
    fn test_cse_scalar_survives_write_accessor() {
        // Scalar intrinsic results are unaffected by heap writes — still CSE'd
        // v2 = Add(v0, v1); WriteAccessor{...}; v3 = Add(v0, v1) → Copy
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::uint()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::uint()),
            Var::new(
                var(4),
                ast::Identifier("arr".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(1)],
                }),
                si(Instruction::WriteAccessor {
                    base: var(4),
                    key: var(0),
                    value: var(1),
                }),
                si(Instruction::Intrinsic {
                    dest: var(3),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[4].node,
            Instruction::Copy { dest, src } if *dest == var(3) && *src == var(2)
        ));
    }

    #[test]
    fn test_cse_len_invalidated_by_append() {
        // v1 = Len(v0); Append(v3, v0, v2); v4 = Len(v0) ← length changed → no CSE
        let locals = vec![
            Var::new(
                var(0),
                ast::Identifier("arr".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
            Var::new(var(1), ast::Identifier("l1".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("v".into()), TypeSet::uint()),
            Var::new(
                var(3),
                ast::Identifier("arr2".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
            Var::new(var(4), ast::Identifier("l2".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Len,
                    args: vec![var(0)],
                }),
                si(Instruction::Const {
                    dest: var(2),
                    value: Literal::UInt(9),
                }),
                si(Instruction::Append {
                    dest: var(3),
                    arr: var(0),
                    value: var(2),
                }),
                si(Instruction::Intrinsic {
                    dest: var(4),
                    op: IntrinsicOp::Len,
                    args: vec![var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(4)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 0);
    }

    #[test]
    fn test_cse_seq_ops_never_merged() {
        // Two identical MakeSeq(v0, v1) create INDEPENDENT sequences — merging
        // them would share one advancing cursor. Never CSE'd.
        let locals = vec![
            Var::new(var(0), ast::Identifier("s".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("e".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("q1".into()), TypeSet::any()),
            Var::new(var(3), ast::Identifier("q2".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(0),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(3),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::MakeSeq,
                    args: vec![var(0), var(1)],
                }),
                si(Instruction::Intrinsic {
                    dest: var(3),
                    op: IntrinsicOp::MakeSeq,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 0);
    }

    #[test]
    fn test_cse_duplicate_index() {
        // v2 = Index(v0, v1)
        // v3 = Index(v0, v1) ← same base+key → Copy(v3, v2)
        let locals = vec![
            Var::new(
                var(0),
                ast::Identifier("arr".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
            Var::new(var(1), ast::Identifier("idx".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::any()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Index {
                    dest: var(2),
                    base: var(0),
                    key: var(1),
                }),
                si(Instruction::Index {
                    dest: var(3),
                    base: var(0),
                    key: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(3)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = eliminate_common_subexpressions(&mut func);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Copy { dest, src } if *dest == var(3) && *src == var(2)
        ));
    }
}
