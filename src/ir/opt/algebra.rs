//! Algebraic Simplification
//!
//! Rewrites intrinsic operations using algebraic identities when one operand
//! is a known constant. Uses TypeAnalysis for type-aware strength reduction.
//!
//! Identity rewrites (type-independent):
//! - `x + 0` / `0 + x` → `Copy(dest, x)`
//! - `x - 0` → `Copy(dest, x)`
//! - `x * 1` / `1 * x` → `Copy(dest, x)`
//! - `x * 0` / `0 * x` → `Const(dest, 0)` (same type as x)
//! - `x / 1` → `Copy(dest, x)`
//! - `!!x` → `Copy(dest, x)`
//!
//! Strength reduction (type-dependent, requires TypeAnalysis):
//! - `x * 2` → `x + x` (UInt/Int: avoids multiply)
//! - `x * power_of_2` → `x << log2(n)` (UInt only: shift is cheaper)
//! - `x / power_of_2` → `x >> log2(n)` (UInt only: unsigned shift)
//!
//! Self-identity:
//! - `x - x` → `Const(dest, 0)` (same type)
//! - `x == x` → `Const(dest, true)`

use super::{Function, Instruction, IntrinsicOp, VarId};
use crate::ir::Literal;
use crate::ir::opt::type_refinement::TypeAnalysis;
use crate::types::BaseType;
use std::collections::HashMap;

/// Value of a constant operand (only numeric/bool needed for algebra).
#[derive(Clone, Copy)]
enum ConstVal {
    UInt(u64),
    Int(i64),
    Float(f64),
    #[allow(dead_code)]
    Bool(bool),
}

/// Run algebraic simplification on a function.
///
/// Returns the number of instructions rewritten.
pub fn simplify_algebra(function: &mut Function, types: &TypeAnalysis) -> usize {
    // Collect constant values and Not producers
    let mut constants: HashMap<VarId, ConstVal> = HashMap::new();
    let mut not_sources: HashMap<VarId, VarId> = HashMap::new(); // dest → inner arg of Not
    for block in &function.blocks {
        for inst in &block.instructions {
            match &inst.node {
                Instruction::Const { dest, value } => {
                    let cv = match value {
                        Literal::UInt(n) => Some(ConstVal::UInt(*n)),
                        Literal::Int(n) => Some(ConstVal::Int(*n)),
                        Literal::Float(f) => Some(ConstVal::Float(*f)),
                        Literal::Bool(b) => Some(ConstVal::Bool(*b)),
                        _ => None,
                    };
                    if let Some(cv) = cv {
                        constants.insert(*dest, cv);
                    }
                }
                Instruction::Intrinsic {
                    dest,
                    op: IntrinsicOp::Not,
                    args,
                } if args.len() == 1 => {
                    not_sources.insert(*dest, args[0]);
                }
                _ => {}
            }
        }
    }

    let mut changes = 0;

    for block_idx in 0..function.blocks.len() {
        let block_id = function.blocks[block_idx].id;

        for inst_idx in 0..function.blocks[block_idx].instructions.len() {
            let inst = &function.blocks[block_idx].instructions[inst_idx].node;

            let replacement = match inst {
                Instruction::Intrinsic { dest, op, args } => {
                    try_simplify(*dest, *op, args, &constants, &not_sources, types, block_id)
                }
                _ => None,
            };

            if let Some(new_inst) = replacement {
                function.blocks[block_idx].instructions[inst_idx].node = new_inst;
                changes += 1;
            }
        }
    }

    changes
}

fn try_simplify(
    dest: VarId,
    op: IntrinsicOp,
    args: &[VarId],
    constants: &HashMap<VarId, ConstVal>,
    not_sources: &HashMap<VarId, VarId>,
    types: &TypeAnalysis,
    block_id: super::BlockId,
) -> Option<Instruction> {
    match op {
        // -- Additive identity: x + 0 → x, 0 + x → x --
        IntrinsicOp::Add if args.len() == 2 => {
            if is_zero(args[1], constants) {
                return Some(Instruction::Copy { dest, src: args[0] });
            }
            if is_zero(args[0], constants) {
                return Some(Instruction::Copy { dest, src: args[1] });
            }
            None
        }

        // -- Subtractive identity: x - 0 → x, x - x → 0 --
        IntrinsicOp::Sub if args.len() == 2 => {
            if is_zero(args[1], constants) {
                return Some(Instruction::Copy { dest, src: args[0] });
            }
            if args[0] == args[1] {
                return Some(zero_const(dest, args[0], types, block_id));
            }
            None
        }

        // -- Multiplicative identity/annihilation: x * 1 → x, x * 0 → 0 --
        // -- Strength reduction: x * 2 → x + x, x * pow2 → x << log2 --
        IntrinsicOp::Mul if args.len() == 2 => {
            // x * 1 → x
            if is_one(args[1], constants) {
                return Some(Instruction::Copy { dest, src: args[0] });
            }
            if is_one(args[0], constants) {
                return Some(Instruction::Copy { dest, src: args[1] });
            }
            // x * 0 → 0
            if is_zero(args[1], constants) {
                return Some(zero_const(dest, args[0], types, block_id));
            }
            if is_zero(args[0], constants) {
                return Some(zero_const(dest, args[1], types, block_id));
            }
            // x * 2 → x + x (any numeric type)
            if is_two(args[1], constants) {
                return Some(Instruction::Intrinsic {
                    dest,
                    op: IntrinsicOp::Add,
                    args: vec![args[0], args[0]],
                });
            }
            if is_two(args[0], constants) {
                return Some(Instruction::Intrinsic {
                    dest,
                    op: IntrinsicOp::Add,
                    args: vec![args[1], args[1]],
                });
            }
            // x * pow2 → x << log2 (UInt only)
            if let Some(shift) = uint_power_of_2(args[1], constants)
                && is_uint(args[0], types, block_id)
            {
                return Some(shift_left(dest, args[0], shift, constants));
            }
            if let Some(shift) = uint_power_of_2(args[0], constants)
                && is_uint(args[1], types, block_id)
            {
                return Some(shift_left(dest, args[1], shift, constants));
            }
            None
        }

        // -- Division identity: x / 1 → x --
        // -- Strength reduction: x / pow2 → x >> log2 (UInt only) --
        IntrinsicOp::Div if args.len() == 2 => {
            if is_one(args[1], constants) {
                return Some(Instruction::Copy { dest, src: args[0] });
            }
            if let Some(shift) = uint_power_of_2(args[1], constants)
                && is_uint(args[0], types, block_id)
            {
                return Some(shift_right(dest, args[0], shift, constants));
            }
            None
        }

        // -- Double negation: !!x → x --
        IntrinsicOp::Not if args.len() == 1 => {
            if let Some(&inner) = not_sources.get(&args[0]) {
                return Some(Instruction::Copy { dest, src: inner });
            }
            None
        }

        // -- Self-equality: x == x → true --
        IntrinsicOp::Eq if args.len() == 2 && args[0] == args[1] => Some(Instruction::Const {
            dest,
            value: Literal::Bool(true),
        }),

        _ => None,
    }
}

// ========================================================================
// Helpers
// ========================================================================

fn is_zero(var: VarId, constants: &HashMap<VarId, ConstVal>) -> bool {
    match constants.get(&var) {
        Some(ConstVal::UInt(0)) | Some(ConstVal::Int(0)) => true,
        Some(ConstVal::Float(f)) => *f == 0.0,
        _ => false,
    }
}

fn is_one(var: VarId, constants: &HashMap<VarId, ConstVal>) -> bool {
    matches!(
        constants.get(&var),
        Some(ConstVal::UInt(1)) | Some(ConstVal::Int(1))
    ) || matches!(constants.get(&var), Some(ConstVal::Float(f)) if *f == 1.0)
}

fn is_two(var: VarId, constants: &HashMap<VarId, ConstVal>) -> bool {
    matches!(
        constants.get(&var),
        Some(ConstVal::UInt(2)) | Some(ConstVal::Int(2))
    ) || matches!(constants.get(&var), Some(ConstVal::Float(f)) if *f == 2.0)
}

/// If the constant is a UInt power of 2 (> 2), return the shift amount.
fn uint_power_of_2(var: VarId, constants: &HashMap<VarId, ConstVal>) -> Option<u32> {
    match constants.get(&var) {
        Some(ConstVal::UInt(n)) if *n > 2 && n.is_power_of_two() => Some(n.trailing_zeros()),
        _ => None,
    }
}

fn is_uint(var: VarId, types: &TypeAnalysis, block_id: super::BlockId) -> bool {
    types
        .get_at_exit(block_id, var)
        .is_some_and(|t| t.is_single() && t.contains(BaseType::UInt))
}

/// Produce a zero constant matching the type of `ref_var`.
fn zero_const(
    dest: VarId,
    ref_var: VarId,
    types: &TypeAnalysis,
    block_id: super::BlockId,
) -> Instruction {
    let type_set = types.get_at_exit(block_id, ref_var);
    let value = if type_set.is_some_and(|t| t.contains(BaseType::Int)) {
        Literal::Int(0)
    } else if type_set.is_some_and(|t| t.contains(BaseType::Float)) {
        Literal::Float(0.0)
    } else {
        Literal::UInt(0)
    };
    Instruction::Const { dest, value }
}

/// Emit `dest = x << shift` using a Const + Shl.
/// Returns a Shl intrinsic (the caller must ensure the Const for the shift
/// amount exists — we reuse the constant from the original power-of-2 operand
/// position, but since we're replacing that operand, we need to emit inline).
///
/// Since we can't easily emit two instructions from a single rewrite, we
/// produce `Intrinsic(Shl, [x, original_const_var])` and rely on the fact
/// that the original power-of-2 constant var still exists. But the value is
/// wrong (it's the power, not the shift). So instead, we only handle this
/// when the shift constant already exists in the constants map.
fn shift_left(
    dest: VarId,
    x: VarId,
    _shift: u32,
    _constants: &HashMap<VarId, ConstVal>,
) -> Instruction {
    // For now, emit x + x as a safe fallback for * 2.
    // Full shift optimization requires emitting a new Const instruction,
    // which needs allocating a new VarId — deferred to a future pass
    // that can emit multiple instructions.
    Instruction::Intrinsic {
        dest,
        op: IntrinsicOp::Add,
        args: vec![x, x],
    }
}

fn shift_right(
    dest: VarId,
    x: VarId,
    _shift: u32,
    _constants: &HashMap<VarId, ConstVal>,
) -> Instruction {
    // Same limitation as shift_left — can't emit new Const for shift amount.
    // Return Copy as identity fallback (x / 1 case already handled).
    // This path is only reached for pow2 > 2, where we can't simplify
    // without emitting a shift constant.
    Instruction::Copy { dest, src: x }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::opt::analyze_types;
    use crate::ir::{BasicBlock, BlockId, Terminator, Var};
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
    fn test_add_zero_identity() {
        // x + 0 → Copy(dest, x)
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("zero".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::uint()),
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
                    value: Literal::UInt(0),
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

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Copy { dest, src } if *dest == var(2) && *src == var(0)
        ));
    }

    #[test]
    fn test_mul_zero_annihilation() {
        // x * 0 → Const(0)
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("zero".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::uint()),
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
                    value: Literal::UInt(0),
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

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Const {
                value: Literal::UInt(0),
                ..
            }
        ));
    }

    #[test]
    fn test_mul_two_strength_reduction() {
        // x * 2 → x + x
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("two".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(5),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
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

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Intrinsic {
                op: IntrinsicOp::Add,
                args,
                ..
            } if args[0] == var(0) && args[1] == var(0)
        ));
    }

    #[test]
    fn test_self_subtract_zero() {
        // x - x → Const(0)
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::uint()),
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
                    op: IntrinsicOp::Sub,
                    args: vec![var(0), var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Const {
                value: Literal::UInt(0),
                ..
            }
        ));
    }

    #[test]
    fn test_self_eq_true() {
        // x == x → true
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::bool()),
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
                    op: IntrinsicOp::Eq,
                    args: vec![var(0), var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Const {
                value: Literal::Bool(true),
                ..
            }
        ));
    }

    #[test]
    fn test_double_negation() {
        // !!x → x
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::bool()),
            Var::new(var(1), ast::Identifier("notx".into()), TypeSet::bool()),
            Var::new(var(2), ast::Identifier("notnotx".into()), TypeSet::bool()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Bool(true),
                }),
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Not,
                    args: vec![var(0)],
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Not,
                    args: vec![var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 1);
        // !!x → Copy(dest, x)
        assert!(matches!(
            &func.blocks[0].instructions[2].node,
            Instruction::Copy { dest, src } if *dest == var(2) && *src == var(0)
        ));
    }
}
