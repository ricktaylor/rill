//! Identity Convert Elimination
//!
//! Replaces `Convert(T, _, [v])` with `Copy(dest, v)` when type analysis
//! proves the source value is already type T.
//!
//! Runs after type refinement (needs TypeAnalysis). Returns the number
//! of instructions rewritten, for fixpoint integration.

use crate::ir::{Function, Instruction, IntrinsicOp};
use crate::types::BaseType;

/// Eliminate identity Convert instructions.
///
/// Scans for `Intrinsic(Convert(T, _), [v])` where the source variable's
/// type (from TypeAnalysis) already matches the target type T.
/// Rewrites to `Copy(dest, v)`.
pub fn elide_identity_casts(
    function: &mut Function,
    types: &super::type_refinement::TypeAnalysis,
) -> usize {
    let mut changes = 0;

    for block_idx in 0..function.blocks.len() {
        let _block_id = function.blocks[block_idx].id;

        for inst_idx in 0..function.blocks[block_idx].instructions.len() {
            let inst = &function.blocks[block_idx].instructions[inst_idx].node;

            let (dest, src, target_base) = match inst {
                Instruction::Intrinsic {
                    dest,
                    op: IntrinsicOp::Convert(t, _),
                    args,
                } if !args.is_empty() => (*dest, args[0], BaseType::from(*t)),
                _ => continue,
            };

            // Get source type from analysis
            let src_type = match types.get(src) {
                Some(t) if t.is_single() => t,
                _ => continue,
            };

            // Check if source type matches target
            if src_type.contains(target_base) {
                function.blocks[block_idx].instructions[inst_idx].node =
                    Instruction::Copy { dest, src };
                changes += 1;
            }
        }
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, BlockId, Literal, Terminator, Var, VarId};
    use crate::opt::analyze_types;
    use crate::types::{ConvertMode, NumericType, TypeSet};

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
    fn test_identity_unchecked_eliminated() {
        // Convert(UInt, Unchecked, [UInt_var]) where source is UInt → Copy
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Convert(NumericType::UInt, ConvertMode::Unchecked),
                    args: vec![var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = elide_identity_casts(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Copy { dest, src } if *dest == var(1) && *src == var(0)
        ));
    }

    #[test]
    fn test_non_identity_unchecked_kept() {
        // Convert(Int, Unchecked, [UInt_var]) → not identity, keep
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Unchecked),
                    args: vec![var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = elide_identity_casts(&mut func, &types);

        assert_eq!(changes, 0);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Intrinsic {
                op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Unchecked),
                ..
            }
        ));
    }

    #[test]
    fn test_identity_checked_eliminated() {
        // Convert(Int, Checked, [Int_var]) where source is Int → Copy
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::int()),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Int(-5),
                }),
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Checked),
                    args: vec![var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = elide_identity_casts(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Copy { dest, src } if *dest == var(1) && *src == var(0)
        ));
    }
}
