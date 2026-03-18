//! Common Subexpression Elimination (CSE)
//!
//! Within each basic block, identifies instructions that compute the same
//! result (same opcode + same operands) and replaces duplicates with a
//! Copy of the first computation's result.
//!
//! Safe for pure operations in SSA form: same VarId operands guarantee
//! same values. SetIndex/WriteRef create new VarIds, so intervening
//! mutations don't invalidate earlier Index results.
//!
//! Runs in the Phase 1 fixpoint loop. Copy propagation + DCE clean up
//! the resulting Copy + dead original instruction.

use super::{Function, Instruction, IntrinsicOp, VarId};
use crate::builtins::BuiltinRegistry;
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

/// Eliminate common subexpressions within each basic block.
///
/// Returns the number of instructions replaced with Copy.
pub fn eliminate_common_subexpressions(function: &mut Function) -> usize {
    eliminate_common_subexpressions_with_purity(function, None, &HashSet::new())
}

/// CSE with interprocedural purity information.
///
/// Pure builtin and user function calls with the same args are also CSE'd.
pub fn eliminate_common_subexpressions_with_purity(
    function: &mut Function,
    builtins: Option<&BuiltinRegistry>,
    pure_functions: &HashSet<String>,
) -> usize {
    let mut changes = 0;

    for block in &mut function.blocks {
        // Map from expression key → first VarId that computed it
        let mut seen: HashMap<ExprKey, VarId> = HashMap::new();

        for inst in &mut block.instructions {
            let (dest, key) = match &inst.node {
                Instruction::Intrinsic { dest, op, args } => {
                    // All intrinsics are pure (no side effects) — safe to CSE.
                    // Fallible ops (Add overflow, Div by zero) produce the same
                    // undefined result for the same inputs, so CSE is correct.
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
                    let name = func_ref.qualified_name();
                    let is_pure = if let Some(registry) = builtins {
                        registry
                            .get(&name)
                            .is_some_and(|def| def.meta.purity.is_pure())
                    } else {
                        false
                    } || pure_functions.contains(&name);

                    if !is_pure {
                        continue;
                    }

                    let arg_vars: Vec<VarId> = args.iter().map(|a| a.value).collect();
                    (*dest, ExprKey::Call(name, arg_vars))
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
            name: ast::Identifier("test".into()),
            params: vec![],
            rest_param: None,
            blocks,
            locals,
            entry_block: BlockId(0),
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
            Var::new(var(2), ast::Identifier("r1".into()), TypeSet::all()),
            Var::new(var(3), ast::Identifier("r2".into()), TypeSet::all()),
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
