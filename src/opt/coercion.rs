//! Coercion Insertion Pass
//!
//! Inserts explicit checked `Convert` instructions for mixed-type arithmetic operations,
//! using TypeAnalysis to determine operand types. This makes implicit numeric
//! promotion visible in the IR, enabling the optimizer to fold, hoist, and
//! eliminate coercions.
//!
//! **Transformations:**
//!
//! - **Same known type** (e.g. UInt+UInt): no change — already monomorphic.
//! - **Mixed known types** (e.g. UInt+Int): insert checked `Convert` for the narrower
//!   operand, rewrite the op to use the widened value.
//! - **Incompatible types** (e.g. Text+UInt): replace with `Undefined` — the
//!   operation provably cannot succeed.
//! - **Unknown/multi-type**: leave as-is — full runtime dispatch.
//!
//! Runs after type refinement (Phase 2). After coercion insertion, the Phase 1
//! fixpoint loop re-runs on the expanded IR: const fold collapses
//! `Convert(Int, Checked, [Const(42_u64)])` → `Const(42_i64)`, definedness sees the new
//! `Undefined` instructions, guard elim + CFG simplify clean up dead branches.

use crate::ast;
use crate::ir::{BlockId, Function, Instruction, IntrinsicOp, VarId};
use crate::ir::{SpannedInst, Var};
use crate::opt::type_refinement::TypeAnalysis;
use crate::types::{BaseType, ConvertMode, NumericType, TypeSet};
use std::collections::HashMap;

/// Insert explicit checked Convert instructions for mixed-type arithmetic.
///
/// Returns the number of instructions modified.
pub fn insert_coercions(function: &mut Function, types: &TypeAnalysis) -> usize {
    let mut next_id = function
        .locals
        .iter()
        .map(|v| v.id.0 + 1)
        .max()
        .unwrap_or(0);
    let mut changes = 0;
    let mut new_locals: Vec<Var> = Vec::new();

    for block in &mut function.blocks {
        let block_id = block.id;
        let old_instructions = std::mem::take(&mut block.instructions);
        let mut new_instructions: Vec<SpannedInst> = Vec::with_capacity(old_instructions.len());

        for inst in &old_instructions {
            match &inst.node {
                Instruction::Intrinsic { dest, op, args }
                    if is_coercible_binary(*op) && args.len() == 2 =>
                {
                    let a_type = lookup_type(types, block_id, args[0]);
                    let b_type = lookup_type(types, block_id, args[1]);

                    let numeric = TypeSet::numeric();
                    let a_numeric = a_type.intersection(&numeric);
                    let b_numeric = b_type.intersection(&numeric);

                    // Incompatible: if either arg is provably non-numeric
                    // (known type with no numeric intersection), the op
                    // always produces undefined.
                    let a_incompatible = a_numeric.is_empty() && !a_type.is_empty();
                    let b_incompatible = b_numeric.is_empty() && !b_type.is_empty();
                    if a_incompatible || b_incompatible {
                        new_instructions.push(spanned(
                            Instruction::Const {
                                dest: *dest,
                                value: crate::ir::Literal::Undefined,
                            },
                            inst.span,
                        ));
                        changes += 1;
                        continue;
                    }

                    // Both single known numeric type — try coercion
                    if a_type.is_single() && b_type.is_single() {
                        let a_base = single_numeric(a_type);
                        let b_base = single_numeric(b_type);

                        if let (Some(a), Some(b)) = (a_base, b_base) {
                            if a == b {
                                // Same type → no coercion needed
                                new_instructions.push(inst.clone());
                                continue;
                            }

                            // Mixed types → widen the narrower operand
                            let target = promote(a, b);
                            let widen_idx = if a != target { 0 } else { 1 };

                            let (widened, widen_insts) = emit_widen(
                                args[widen_idx],
                                target,
                                inst.span,
                                &mut next_id,
                                &mut new_locals,
                            );
                            new_instructions.extend(widen_insts);

                            let mut new_args = args.clone();
                            new_args[widen_idx] = widened;
                            new_instructions.push(spanned(
                                Instruction::Intrinsic {
                                    dest: *dest,
                                    op: *op,
                                    args: new_args,
                                },
                                inst.span,
                            ));
                            changes += 1;
                            continue;
                        }
                    }

                    // Unknown or multi-type → leave as-is
                    new_instructions.push(inst.clone());
                }
                _ => new_instructions.push(inst.clone()),
            }
        }

        block.instructions = new_instructions;
    }

    function.locals.extend(new_locals);
    changes
}

/// Binary ops that benefit from coercion insertion.
fn is_coercible_binary(op: IntrinsicOp) -> bool {
    matches!(
        op,
        IntrinsicOp::Add
            | IntrinsicOp::Sub
            | IntrinsicOp::Mul
            | IntrinsicOp::Div
            | IntrinsicOp::Mod
            | IntrinsicOp::Lt
    )
}

/// Look up a variable's type from the analysis, defaulting to all types.
fn lookup_type(types: &TypeAnalysis, _block: BlockId, var: VarId) -> TypeSet {
    types.get(var).copied().unwrap_or(TypeSet::any())
}

/// Extract the single numeric BaseType from a TypeSet, if it contains exactly one.
fn single_numeric(ts: TypeSet) -> Option<BaseType> {
    if !ts.is_single() {
        return None;
    }
    if ts.contains(BaseType::UInt) {
        Some(BaseType::UInt)
    } else if ts.contains(BaseType::Int) {
        Some(BaseType::Int)
    } else if ts.contains(BaseType::Float) {
        Some(BaseType::Float)
    } else {
        None
    }
}

/// Determine the promotion target for two numeric types.
/// Promotion lattice: UInt < Int < Float.
fn promote(a: BaseType, b: BaseType) -> BaseType {
    match (a, b) {
        (BaseType::Float, _) | (_, BaseType::Float) => BaseType::Float,
        (BaseType::Int, _) | (_, BaseType::Int) => BaseType::Int,
        _ => BaseType::UInt,
    }
}

/// Emit a checked Convert instruction for implicit numeric promotion.
/// Returns the widened VarId and the instruction to insert.
fn emit_widen(
    value: VarId,
    target: BaseType,
    span: ast::Span,
    next_id: &mut u32,
    new_locals: &mut Vec<Var>,
) -> (VarId, Vec<SpannedInst>) {
    let target_nt = NumericType::from(target);

    let widened = VarId(*next_id);
    *next_id += 1;
    new_locals.push(Var::new(
        widened,
        ast::Identifier("$widen".to_string()),
        TypeSet::single(target),
    ));

    let insts = vec![spanned(
        Instruction::Intrinsic {
            dest: widened,
            op: IntrinsicOp::Convert(target_nt, ConvertMode::Checked),
            args: vec![value],
        },
        span,
    )];

    (widened, insts)
}

fn spanned(inst: Instruction, span: ast::Span) -> SpannedInst {
    ast::Spanned::new(inst, span)
}

// ============================================================================
// Redundant Coercion Elimination
// ============================================================================

/// Metadata for a checked Convert instruction: what value it converts and to what target.
struct ConvertInfo {
    /// The original input value (before conversion)
    original: VarId,
    /// The target type
    target: NumericType,
}

/// Eliminate redundant checked Convert instructions.
///
/// Two rewrites:
///
/// 1. **Chain collapsing**: `Convert(Float, Checked, [Convert(Int, Checked, [x])])`
///    → `Convert(Float, Checked, [x])`. Skips the intermediate type — widening
///    is transitive along the lattice.
///
/// 2. **Identity elimination**: `Convert(T, Checked, [v])` where `v` was produced
///    by `Convert(T, Checked, _)` → `Copy(dest, v)`. Already the target type.
///
/// Only operates on `Checked` conversions (compiler-inserted promotions).
/// Runs in the Phase 1 fixpoint loop. No TypeAnalysis needed — works purely
/// on instruction structure. Returns the number of instructions rewritten.
pub fn elide_coercions(function: &mut Function) -> usize {
    // Phase 1: Collect checked Convert metadata
    let mut convert_info: HashMap<VarId, ConvertInfo> = HashMap::new();

    for block in &function.blocks {
        for inst in &block.instructions {
            if let Instruction::Intrinsic {
                dest,
                op: IntrinsicOp::Convert(target, ConvertMode::Checked),
                args,
            } = &inst.node
                && let Some(&input) = args.first()
            {
                convert_info.insert(
                    *dest,
                    ConvertInfo {
                        original: input,
                        target: *target,
                    },
                );
            }
        }
    }

    if convert_info.is_empty() {
        return 0;
    }

    // Phase 2: Rewrite
    let mut changes = 0;

    for block in &mut function.blocks {
        for inst in &mut block.instructions {
            let Instruction::Intrinsic {
                dest,
                op: IntrinsicOp::Convert(target, ConvertMode::Checked),
                args,
            } = &inst.node
            else {
                continue;
            };

            let dest = *dest;
            let target = *target;
            let input = args[0];

            // Check if the input was produced by another checked Convert
            if let Some(inner) = convert_info.get(&input) {
                if inner.target == target {
                    // Identity: input is already the target type
                    inst.node = Instruction::Copy { dest, src: input };
                    changes += 1;
                } else {
                    // Chain: skip intermediate conversion
                    let mut root = inner.original;
                    while let Some(deeper) = convert_info.get(&root) {
                        root = deeper.original;
                    }
                    inst.node = Instruction::Intrinsic {
                        dest,
                        op: IntrinsicOp::Convert(target, ConvertMode::Checked),
                        args: vec![root],
                    };
                    changes += 1;
                }
            }
        }
    }

    changes
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{BasicBlock, Literal, Param, Terminator};
    use crate::opt::analyze_types;

    fn var(id: u32) -> VarId {
        VarId(id)
    }

    fn block_id(id: u32) -> BlockId {
        BlockId(id)
    }

    fn si(inst: Instruction) -> SpannedInst {
        ast::Spanned::new(inst, ast::Span::default())
    }

    fn make_function_with_locals(blocks: Vec<BasicBlock>, locals: Vec<Var>) -> Function {
        Function {
            blocks,
            locals,
            ..Default::default()
        }
    }

    #[test]
    fn test_same_type_no_coercion() {
        // Add(UInt, UInt) → no change
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
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
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = insert_coercions(&mut func, &types);

        assert_eq!(changes, 0);
        // Instruction should be unchanged
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Intrinsic {
                op: IntrinsicOp::Add,
                args,
                ..
            } if args.len() == 2 && args[0] == var(0) && args[1] == var(1)
        ));
    }

    #[test]
    fn test_mixed_uint_int_inserts_widen() {
        // Add(UInt, Int) → Widen(UInt→Int) + Add(Int, Int)
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::int()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::Int(2),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = insert_coercions(&mut func, &types);

        assert_eq!(changes, 1);
        // Should have 4 instructions now: 2 Const + Widen + Add
        assert_eq!(func.blocks[0].instructions.len(), 4);

        // The Widen should target Int
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Intrinsic {
                op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Checked),
                args,
                ..
            } if args.len() == 1
        ));
        // The Add should use the widened arg for arg[0]
        if let Instruction::Intrinsic { args, .. } = &func.blocks[0].instructions[3].node {
            assert_ne!(args[0], var(0)); // arg[0] is now the widened value
            assert_eq!(args[1], var(1)); // arg[1] unchanged
        } else {
            panic!("expected Intrinsic");
        }
    }

    #[test]
    fn test_incompatible_types_becomes_undefined() {
        // Add(Bool, UInt) → Undefined (Bool has no intersection with numeric)
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::bool()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Bool(true),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(5),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = insert_coercions(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Const { dest, value: Literal::Undefined } if *dest == var(2)
        ));
    }

    #[test]
    fn test_mixed_int_float_inserts_widen() {
        // Mul(Int, Float) → Widen(Int→Float) + Mul(Float, Float)
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::int()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::float()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Int(3),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::Float(2.5),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Mul,
                    args: vec![var(0), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = insert_coercions(&mut func, &types);

        assert_eq!(changes, 1);
        // Widen target should be Float
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Intrinsic {
                op: IntrinsicOp::Convert(NumericType::Float, ConvertMode::Checked),
                ..
            }
        ));
    }

    #[test]
    fn test_unknown_types_left_alone() {
        // Add(param, param) where types are unknown → no change
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::any()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::any()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![si(Instruction::Intrinsic {
                dest: var(2),
                op: IntrinsicOp::Add,
                args: vec![var(0), var(1)],
            })],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let func_with_params = Function {
            params: vec![
                Param {
                    var: var(0),
                    by_ref: false,
                },
                Param {
                    var: var(1),
                    by_ref: false,
                },
            ],
            ..func
        };
        func = func_with_params;
        let types = analyze_types(&func, None);
        let changes = insert_coercions(&mut func, &types);

        assert_eq!(changes, 0);
    }

    // ====================================================================
    // Redundant Coercion Elimination Tests
    // ====================================================================

    #[test]
    fn test_identity_widen_eliminated() {
        // Widen(v1, Int) where v1 = Widen(v0, Int) → Copy(v2, v1)
        // v1 is already Int, so widening to Int again is identity.
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("w1".into()), TypeSet::int()),
            Var::new(var(2), ast::Identifier("w2".into()), TypeSet::int()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                // v1 = Widen(v0, Int)
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Checked),
                    args: vec![var(0)],
                }),
                // v2 = Widen(v1, Int) — identity! v1 is already Int
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Checked),
                    args: vec![var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let changes = elide_coercions(&mut func);

        assert_eq!(changes, 1);
        // v2 should now be Copy(v2, v1)
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Copy { dest, src }
                if *dest == var(2) && *src == var(1)
        ));
    }

    #[test]
    fn test_chain_collapsed() {
        // Widen(Widen(v0, Int), Float) → Widen(v0, Float)
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("w1".into()), TypeSet::int()),
            Var::new(var(2), ast::Identifier("w2".into()), TypeSet::float()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                // v1 = Widen(v0, Int) — UInt → Int
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Convert(NumericType::Int, ConvertMode::Checked),
                    args: vec![var(0)],
                }),
                // v2 = Widen(v1, Float) — should become Widen(v0, Float)
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Convert(NumericType::Float, ConvertMode::Checked),
                    args: vec![var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let changes = elide_coercions(&mut func);

        assert_eq!(changes, 1);
        // v2 should now be Widen(v0, Float) — skips v1
        if let Instruction::Intrinsic { dest, op, args } = &func.blocks[0].instructions[2].node {
            assert_eq!(*dest, var(2));
            assert_eq!(
                *op,
                IntrinsicOp::Convert(NumericType::Float, ConvertMode::Checked)
            );
            assert_eq!(args[0], var(0)); // original input, not v1
        } else {
            panic!("expected Widen intrinsic");
        }
    }

    #[test]
    fn test_no_widen_no_changes() {
        // No Widen instructions → 0 changes
        let locals = vec![
            Var::new(var(0), ast::Identifier("a".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("b".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block_id(0),
            instructions: vec![si(Instruction::Intrinsic {
                dest: var(2),
                op: IntrinsicOp::Add,
                args: vec![var(0), var(1)],
            })],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function_with_locals(blocks, locals);
        let changes = elide_coercions(&mut func);
        assert_eq!(changes, 0);
    }
}
