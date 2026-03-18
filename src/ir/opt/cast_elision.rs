//! Identity Cast/Widen Elimination
//!
//! Replaces `Cast(v, T)` and `Widen(v, T)` with `Copy(dest, v)` when
//! type analysis proves the source value is already type T.
//!
//! Runs after type refinement (needs TypeAnalysis). Returns the number
//! of instructions rewritten, for fixpoint integration.

use super::{Function, Instruction, IntrinsicOp, VarId};
use crate::ir::Literal;
use crate::types::BaseType;

/// Eliminate identity Cast and Widen instructions.
///
/// Scans for `Intrinsic(Cast/Widen, [v, target_const])` where the source
/// variable's type (from TypeAnalysis) already matches the target type.
/// Rewrites to `Copy(dest, v)`.
pub fn elide_identity_casts(
    function: &mut Function,
    types: &super::type_refinement::TypeAnalysis,
) -> usize {
    // Collect constant UInt values for target resolution
    let mut const_values: std::collections::HashMap<VarId, u64> = std::collections::HashMap::new();
    for block in &function.blocks {
        for inst in &block.instructions {
            if let Instruction::Const {
                dest,
                value: Literal::UInt(n),
            } = &inst.node
            {
                const_values.insert(*dest, *n);
            }
        }
    }

    let mut changes = 0;

    for block_idx in 0..function.blocks.len() {
        let block_id = function.blocks[block_idx].id;

        for inst_idx in 0..function.blocks[block_idx].instructions.len() {
            let inst = &function.blocks[block_idx].instructions[inst_idx].node;

            let (dest, src, target_var) = match inst {
                Instruction::Intrinsic {
                    dest,
                    op: IntrinsicOp::Cast | IntrinsicOp::Widen,
                    args,
                } if args.len() == 2 => (*dest, args[0], args[1]),
                _ => continue,
            };

            // Resolve the target type code
            let target = match const_values.get(&target_var) {
                Some(t) => *t,
                None => continue,
            };

            // Get source type from analysis
            let src_type = match types.get_at_exit(block_id, src) {
                Some(t) if t.is_single() => t,
                _ => continue,
            };

            // Check if source type matches target
            let is_identity = match target {
                1 => src_type.contains(BaseType::UInt),
                2 => src_type.contains(BaseType::Int),
                3 => src_type.contains(BaseType::Float),
                _ => false,
            };

            if is_identity {
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
    use crate::ir::opt::analyze_types;
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
    fn test_identity_cast_eliminated() {
        // Cast(UInt_var, 1) where source is UInt → should become Copy
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("t".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::all()),
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
                    value: Literal::UInt(1), // target = UInt
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Cast,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = elide_identity_casts(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Copy { dest, src } if *dest == var(2) && *src == var(0)
        ));
    }

    #[test]
    fn test_non_identity_cast_kept() {
        // Cast(UInt_var, 2) where source is UInt, target is Int → not identity, keep
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("t".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::all()),
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
                    value: Literal::UInt(2), // target = Int
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Cast,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = elide_identity_casts(&mut func, &types);

        assert_eq!(changes, 0);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Intrinsic {
                op: IntrinsicOp::Cast,
                ..
            }
        ));
    }

    #[test]
    fn test_identity_widen_eliminated() {
        // Widen(Int_var, 2) where source is Int → identity, should become Copy
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::int()),
            Var::new(var(1), ast::Identifier("t".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::all()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Int(-5),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2), // target = Int
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Widen,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = elide_identity_casts(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Copy { dest, src } if *dest == var(2) && *src == var(0)
        ));
    }
}
