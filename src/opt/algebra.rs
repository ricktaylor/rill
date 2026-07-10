//! Algebraic Simplification
//!
//! Rewrites intrinsic operations using algebraic identities when one operand
//! is a known constant.
//!
//! Every rewrite is gated on TypeAnalysis proving the variable operand a
//! SINGLE numeric type matching the constant operand's type:
//! - a possibly-undefined operand must reach the runtime guard — folding
//!   would manufacture a defined result from a failed computation
//! - mixed-type operands promote at runtime (`Int + 0.0` is a Float
//!   addition), so a Copy of the unpromoted operand has the wrong type
//!
//! The `Float` invariant (finite, single +0.0 zero) makes these folds
//! bit-exact for floats too: no NaN, no infinities and no -0.0 means
//! `x + 0`, `x - x` and `x * 0` have no IEEE edge cases left.
//!
//! Rewrites (UInt/Int/Float):
//! - `x + 0` / `0 + x` → `Copy(dest, x)`
//! - `x - 0` → `Copy(dest, x)`
//! - `x * 1` / `1 * x` → `Copy(dest, x)`
//! - `x / 1` → `Copy(dest, x)`
//! - `x * 0` / `0 * x` → `Const(dest, 0)` (same type as x)
//! - `x * 2` / `2 * x` → `x + x`
//! - `x - x` → `Const(dest, 0)` (same type as x)
//! - `x == x` → `Const(dest, true)` (any provably-defined operand)
//! - `!!x` → `Copy(dest, x)`
//!
//! `x * 2^k → x << k` strength reduction is deliberately absent: a rewrite
//! here can only replace one instruction in place, and the shift amount
//! would need a fresh Const. That fusion belongs to a pass that can emit
//! instructions (the planned StepKind peephole layer).

use crate::ir::Literal;
use crate::ir::{Function, Instruction, IntrinsicOp, VarId};
use crate::opt::type_refinement::TypeAnalysis;
use crate::types::BaseType;
use std::collections::HashMap;

/// Value of a constant operand (numeric only — Bool constants don't
/// participate in algebraic rewrites since And/Or are control flow).
#[derive(Clone, Copy)]
enum ConstVal {
    UInt(u64),
    Int(i64),
    Float(f64),
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
        for inst_idx in 0..function.blocks[block_idx].instructions.len() {
            let inst = &function.blocks[block_idx].instructions[inst_idx].node;

            let replacement = match inst {
                Instruction::Intrinsic { dest, op, args } => {
                    try_simplify(*dest, *op, args, &constants, &not_sources, types)
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
) -> Option<Instruction> {
    match op {
        // -- Additive identity: x + 0 → x, 0 + x → x --
        IntrinsicOp::Add if args.len() == 2 => {
            for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                if let Some(t) = single_numeric(x, types)
                    && is_typed_const(c, t, 0, constants)
                {
                    return Some(Instruction::Copy { dest, src: x });
                }
            }
            None
        }

        // -- Subtractive identity: x - 0 → x, x - x → 0 --
        IntrinsicOp::Sub if args.len() == 2 => {
            if let Some(t) = single_numeric(args[0], types) {
                if is_typed_const(args[1], t, 0, constants) {
                    return Some(Instruction::Copy { dest, src: args[0] });
                }
                if args[0] == args[1] {
                    return Some(typed_zero(dest, t));
                }
            }
            None
        }

        // -- Multiplicative identity/annihilation/doubling --
        IntrinsicOp::Mul if args.len() == 2 => {
            for (x, c) in [(args[0], args[1]), (args[1], args[0])] {
                if let Some(t) = single_numeric(x, types) {
                    // x * 1 → x
                    if is_typed_const(c, t, 1, constants) {
                        return Some(Instruction::Copy { dest, src: x });
                    }
                    // x * 0 → 0
                    if is_typed_const(c, t, 0, constants) {
                        return Some(typed_zero(dest, t));
                    }
                    // x * 2 → x + x (overflow to Undefined agrees both ways)
                    if is_typed_const(c, t, 2, constants) {
                        return Some(Instruction::Intrinsic {
                            dest,
                            op: IntrinsicOp::Add,
                            args: vec![x, x],
                        });
                    }
                }
            }
            None
        }

        // -- Division identity: x / 1 → x --
        IntrinsicOp::Div if args.len() == 2 => {
            if let Some(t) = single_numeric(args[0], types)
                && is_typed_const(args[1], t, 1, constants)
            {
                return Some(Instruction::Copy { dest, src: args[0] });
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

        // -- Self-equality: x == x → true (only when x is provably defined:
        // -- the runtime guards Eq operands, so an Undefined x yields
        // -- Undefined, not true) --
        IntrinsicOp::Eq
            if args.len() == 2
                && args[0] == args[1]
                && types.get(args[0]).is_some_and(|t| t.is_defined()) =>
        {
            Some(Instruction::Const {
                dest,
                value: Literal::Bool(true),
            })
        }

        _ => None,
    }
}

// ========================================================================
// Helpers
// ========================================================================

/// The variable operand's proven single numeric type, if any.
fn single_numeric(var: VarId, types: &TypeAnalysis) -> Option<BaseType> {
    types
        .get(var)
        .and_then(|t| t.as_single())
        .filter(|t| matches!(t, BaseType::UInt | BaseType::Int | BaseType::Float))
}

/// True when the constant operand holds `n` in EXACTLY the type `t` — a
/// constant of another numeric type promotes the operation at runtime.
fn is_typed_const(var: VarId, t: BaseType, n: u8, constants: &HashMap<VarId, ConstVal>) -> bool {
    match (constants.get(&var), t) {
        (Some(ConstVal::UInt(v)), BaseType::UInt) => *v == n as u64,
        (Some(ConstVal::Int(v)), BaseType::Int) => *v == n as i64,
        (Some(ConstVal::Float(v)), BaseType::Float) => *v == n as f64,
        _ => false,
    }
}

/// Zero constant of type `t` (the Float zero is unique: always +0.0).
fn typed_zero(dest: VarId, t: BaseType) -> Instruction {
    let value = match t {
        BaseType::Int => Literal::Int(0),
        BaseType::Float => Literal::Float(0.0),
        _ => Literal::UInt(0),
    };
    Instruction::Const { dest, value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, BlockId, Terminator, Var};
    use crate::opt::analyze_types;
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
    fn test_float_self_subtract_folds_to_zero() {
        // Float x - x folds to +0.0: the Float invariant (finite, single
        // zero) leaves no inf - inf or -0.0 edge cases
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::float()),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::float()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Float(1.5),
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
                value: Literal::Float(f),
                ..
            } if *f == 0.0 && f.is_sign_positive()
        ));
    }

    #[test]
    fn test_mixed_type_zero_not_folded() {
        // Int x + UInt 0 must NOT fold to Copy: runtime promotes the pair
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::int()),
            Var::new(var(1), ast::Identifier("zero".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Int(-3),
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

        assert_eq!(changes, 0);
    }

    #[test]
    fn test_self_eq_possibly_undefined_not_folded() {
        // x == x where x may be Undefined must NOT fold to true: the runtime
        // guard yields Undefined for an undefined operand
        let locals = vec![
            Var::new(
                var(0),
                ast::Identifier("x".into()),
                TypeSet::uint().union(&TypeSet::undefined()),
            ),
            Var::new(var(1), ast::Identifier("r".into()), TypeSet::any()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Intrinsic {
                dest: var(1),
                op: IntrinsicOp::Eq,
                args: vec![var(0), var(0)],
            })],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = analyze_types(&func, None);
        let changes = simplify_algebra(&mut func, &types);

        assert_eq!(changes, 0);
    }

    #[test]
    fn test_float_mul_one_still_folds() {
        // x * 1.0 IS bit-exact for floats (including -0.0) — keep folding
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::float()),
            Var::new(var(1), ast::Identifier("one".into()), TypeSet::float()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::float()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Float(2.5),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::Float(1.0),
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
            Instruction::Copy { dest, src } if *dest == var(2) && *src == var(0)
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
