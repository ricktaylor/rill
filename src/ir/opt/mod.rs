//! IR Optimization Passes
//!
//! The optimization pipeline runs after lowering to improve the IR before
//! execution. Passes are ordered to maximize effectiveness:
//!
//! 1. Constant Folding (early) - fold obvious compile-time constants
//! 2. Definedness Analysis - compute which values are provably defined
//! 3. Diagnostics - emit warnings/errors based on definedness
//! 4. Guard Elimination - remove Guards for provably-defined values
//! 5. CFG Simplification - merge blocks, remove unreachable code
//! 6. Type Refinement - narrow TypeSets based on control flow
//! 7. Constant Folding (cleanup) - fold constants exposed by earlier passes
//! 8. Dead Code Elimination - remove unused computations

mod algebra;
mod cast_elision;
mod coercion;
mod const_fold;
mod copy_prop;
mod dce;
mod definedness;
mod guard_elim;
mod ref_elision;
mod type_refinement;

pub use algebra::simplify_algebra;
pub use cast_elision::elide_identity_casts;
pub use coercion::{elide_coercions, insert_coercions};
pub use const_fold::fold_constants;
pub use copy_prop::propagate_copies;
pub use dce::eliminate_dead_code;
pub use definedness::{Definedness, DefinednessAnalysis, analyze_definedness, check_definedness};
pub use guard_elim::{eliminate_guards, simplify_cfg};
pub use ref_elision::elide_refs;
pub use type_refinement::{ReturnTypes, TypeAnalysis, analyze_types, infer_return_type};

// Import IR types from parent module
use super::{
    BlockId, CallArg, Function, FunctionRef, Instruction, IntrinsicOp, IrProgram, Literal,
    MatchPattern, Terminator, VarId,
};

// Import builtins for metadata lookup
use crate::builtins::BuiltinRegistry;
use crate::diagnostics::Diagnostics;

/// Run all optimization passes on a program
pub fn optimize(
    program: &mut IrProgram,
    builtins: &BuiltinRegistry,
    diagnostics: &mut Diagnostics,
) {
    // Phase A: per-function optimization (intraprocedural)
    for function in &mut program.functions {
        optimize_function(function, builtins, diagnostics);
    }

    // Phase B: interprocedural return type inference
    //
    // Iterate until stable: each pass may refine return types, which narrows
    // callers' return types in turn. Handles:
    // - Forward references (fn a calls fn b defined later)
    // - Recursive functions (return type depends on itself)
    // - Mutual recursion (fn a calls fn b calls fn a)
    // Typically converges in 2-3 iterations.
    let mut return_types = ReturnTypes::new();
    loop {
        let mut changed = false;
        for function in &program.functions {
            let rt = infer_return_type(function, Some(builtins), &return_types);
            let name = function.name.to_string();
            let old = return_types
                .get(&name)
                .copied()
                .unwrap_or(crate::types::TypeSet::empty());
            if rt != old {
                return_types.insert(name, rt);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Re-optimize functions that have user function calls, now with narrowed
    // return types feeding into type analysis.
    if !return_types.is_empty() {
        for function in &mut program.functions {
            // Check if this function calls any user function
            let has_user_calls = function.blocks.iter().any(|block| {
                block.instructions.iter().any(|inst| {
                    if let Instruction::Call {
                        function: func_ref, ..
                    } = &inst.node
                    {
                        return_types.contains_key(&func_ref.qualified_name())
                    } else {
                        false
                    }
                })
            });

            if has_user_calls {
                // Re-run Phase 2 with return type info
                let types = type_refinement::analyze_types_with_returns(
                    function,
                    Some(builtins),
                    &return_types,
                );
                let coercions = insert_coercions(function, &types);
                let cast_elisions = elide_identity_casts(function, &types);
                let algebra = simplify_algebra(function, &types);
                let condition_folds = fold_non_bool_conditions(function, &types);
                let dead_arms = eliminate_dead_match_arms(function, &types);

                if coercions + cast_elisions + algebra + condition_folds + dead_arms > 0 {
                    loop {
                        let folded = fold_constants(function, builtins, diagnostics);
                        let copies = propagate_copies(function);
                        let dead = eliminate_dead_code(function);
                        let refs = elide_refs(function);
                        let coerce = elide_coercions(function);
                        let definedness = analyze_definedness(function, Some(builtins));
                        let guards = eliminate_guards(function, &definedness);
                        let blocks = simplify_cfg(function);
                        if folded + copies + dead + refs + coerce + guards + blocks == 0 {
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Run all optimization passes on a single function
pub fn optimize_function(
    function: &mut Function,
    builtins: &BuiltinRegistry,
    diagnostics: &mut Diagnostics,
) {
    // ── Phase 1: Optimize to fixpoint ────────────────────────────────────
    //
    // Loop const fold → definedness → guard elim → CFG simplify until
    // no pass makes any changes. Typically converges in 1-2 iterations.
    // Extra iterations handle cascading effects: const fold may expose
    // new Defined values → guard elim removes guards → CFG simplify
    // removes dead blocks → Phi nodes lose sources → new constants.

    let mut first_iteration = true;
    loop {
        let folded = fold_constants(function, builtins, diagnostics);
        let copies = propagate_copies(function);
        let dead = eliminate_dead_code(function);
        let refs = elide_refs(function);
        let coerce = elide_coercions(function);

        let definedness = analyze_definedness(function, Some(builtins));

        // Emit definedness diagnostics only on the first iteration,
        // before guard elimination reshapes the control flow.
        if first_iteration {
            check_definedness(function, &definedness, Some(builtins), diagnostics);
            first_iteration = false;
        }

        let guards = eliminate_guards(function, &definedness);
        let blocks = simplify_cfg(function);

        if folded + copies + dead + refs + coerce + guards + blocks == 0 {
            break;
        }
    }

    // ── Phase 2: Type-informed analysis (on simplified CFG) ────────────

    // Type refinement — intrinsic-aware: Add(UInt, UInt) → {UInt}.
    let types = analyze_types(function, Some(builtins));

    // Type mismatch diagnostics (W009)
    check_intrinsic_types(function, &types, diagnostics);
    check_condition_types(function, &types, diagnostics);

    // Coercion insertion: makes implicit numeric promotion explicit via Widen.
    // Also replaces provably-incompatible operations with Undefined.
    let coercions = insert_coercions(function, &types);

    // Identity cast/widen elimination: replaces Cast(v, T) and Widen(v, T)
    // with Copy when source type already matches target. Catches user-written
    // redundant casts (e.g. `x as UInt` where x is UInt) and Widens that
    // became identity after type narrowing.
    let cast_elisions = elide_identity_casts(function, &types);

    // Algebraic simplification: x+0→x, x*1→x, x*0→0, x-x→0, x==x→true,
    // x*2→x+x, x*pow2→x<<log2 (UInt only).
    let algebra = simplify_algebra(function, &types);

    // Fold If terminators whose condition is provably not Bool → Jump(else).
    // The then-branch becomes unreachable and is cleaned up by simplify_cfg
    // in the fixpoint re-run below.
    let condition_folds = fold_non_bool_conditions(function, &types);

    // Prune Match arms where the scrutinee's type can never match the pattern.
    // A Match with zero surviving arms → Jump(default).
    // A Match with one surviving arm whose type covers the scrutinee → Jump(arm).
    let dead_arms = eliminate_dead_match_arms(function, &types);

    // If any Phase 2 pass changed the IR, re-run Phase 1 fixpoint.
    if coercions + cast_elisions + algebra + condition_folds + dead_arms > 0 {
        loop {
            let folded = fold_constants(function, builtins, diagnostics);
            let copies = propagate_copies(function);
            let dead = eliminate_dead_code(function);
            let refs = elide_refs(function);
            let coerce = elide_coercions(function);
            let definedness = analyze_definedness(function, Some(builtins));
            let guards = eliminate_guards(function, &definedness);
            let blocks = simplify_cfg(function);
            if folded + copies + dead + refs + coerce + guards + blocks == 0 {
                break;
            }
        }
    }

    // ── Phase 3: Cleanup ───────────────────────────────────────────────
    // DCE runs in both fixpoint loops above. Nothing else needed here.
}

/// Warn when intrinsic operand types guarantee the result is always undefined.
///
/// For example, `true + [1, 2]` — Add requires numeric operands, but Bool and
/// Array have no intersection with numeric. The result is always undefined,
/// which is almost certainly a bug.
fn check_intrinsic_types(
    function: &Function,
    types: &type_refinement::TypeAnalysis,
    diagnostics: &mut Diagnostics,
) {
    for block in &function.blocks {
        for inst in &block.instructions {
            let Instruction::Intrinsic { op, args, .. } = &inst.node else {
                continue;
            };

            // Skip variadic ops where param_type doesn't apply per-arg
            if matches!(
                op,
                IntrinsicOp::MakeArray | IntrinsicOp::MakeMap | IntrinsicOp::ArraySeq
            ) {
                continue;
            }

            for (i, arg) in args.iter().enumerate() {
                let required = op.param_type(i);
                let actual = types
                    .get_at_exit(block.id, *arg)
                    .copied()
                    .unwrap_or(crate::types::TypeSet::all());

                if actual.intersection(&required).is_empty() && !actual.is_empty() {
                    diagnostics.warning(
                        crate::diagnostics::DiagnosticCode::W009_TypeMismatch,
                        inst.span,
                        format!(
                            "in function `{}`: {:?} requires {:?} but argument has type {:?} — result is always undefined",
                            function.name, op, required, actual,
                        ),
                    );
                    break; // one warning per instruction is enough
                }
            }
        }
    }
}

/// Fold If terminators whose condition is provably not Bool into Jump(else).
///
/// When type analysis proves the condition can never be Bool, the If always
/// takes the else branch. Replacing with Jump makes the then-branch unreachable,
/// allowing simplify_cfg to eliminate it.
fn fold_non_bool_conditions(
    function: &mut Function,
    types: &type_refinement::TypeAnalysis,
) -> usize {
    let mut changes = 0;
    for block in &mut function.blocks {
        let (else_target, span) = match &block.terminator {
            Terminator::If {
                condition,
                else_target,
                span,
                ..
            } => {
                let cond_type = types
                    .get_at_exit(block.id, *condition)
                    .copied()
                    .unwrap_or(crate::types::TypeSet::all());

                if !cond_type.contains(crate::types::BaseType::Bool) && !cond_type.is_empty() {
                    (*else_target, *span)
                } else {
                    continue;
                }
            }
            _ => continue,
        };

        block.terminator = Terminator::Jump {
            target: else_target,
        };
        // Preserve span for diagnostics by wrapping in Jump
        let _ = span; // span is consumed by the warning in check_condition_types
        changes += 1;
    }
    changes
}

/// Eliminate Match arms that can never match based on type analysis.
///
/// For each arm, check if the scrutinee's TypeSet intersects the arm's pattern type.
/// Dead arms are removed. If no arms survive, the Match becomes Jump(default).
/// If one arm survives and the scrutinee's type is fully covered by that arm,
/// the Match becomes Jump(arm_target).
fn eliminate_dead_match_arms(
    function: &mut Function,
    types: &type_refinement::TypeAnalysis,
) -> usize {
    let mut changes = 0;

    for block in &mut function.blocks {
        let (value, arms, default, span) = match &block.terminator {
            Terminator::Match {
                value,
                arms,
                default,
                span,
            } => (*value, arms.clone(), *default, *span),
            _ => continue,
        };

        let scrutinee_type = match types.get_at_exit(block.id, value) {
            Some(ts) if !ts.is_empty() => *ts,
            _ => continue, // unknown type — can't prune
        };

        let original_count = arms.len();

        // Filter to surviving arms
        let surviving: Vec<(MatchPattern, BlockId)> = arms
            .into_iter()
            .filter(|(pattern, _)| pattern_can_match(&scrutinee_type, pattern))
            .collect();

        if surviving.len() == original_count {
            continue; // nothing pruned
        }

        if surviving.is_empty() {
            // No arms can match → Jump to default
            block.terminator = Terminator::Jump { target: default };
            changes += 1;
        } else if surviving.len() == 1 && pattern_covers_type(&scrutinee_type, &surviving[0].0) {
            // One arm fully covers the scrutinee type → Jump to that arm
            block.terminator = Terminator::Jump {
                target: surviving[0].1,
            };
            changes += 1;
        } else {
            // Reduced arms — rebuild the Match
            block.terminator = Terminator::Match {
                value,
                arms: surviving,
                default,
                span,
            };
            changes += 1;
        }
    }

    changes
}

/// Can this pattern ever match a value from the given TypeSet?
fn pattern_can_match(type_set: &crate::types::TypeSet, pattern: &MatchPattern) -> bool {
    match pattern {
        MatchPattern::Type(ty) => type_set.contains(*ty),
        MatchPattern::Literal(lit) => {
            let lit_type = match lit {
                Literal::Bool(_) => crate::types::BaseType::Bool,
                Literal::UInt(_) => crate::types::BaseType::UInt,
                Literal::Int(_) => crate::types::BaseType::Int,
                Literal::Float(_) => crate::types::BaseType::Float,
                Literal::Text(_) => crate::types::BaseType::Text,
                Literal::Bytes(_) => crate::types::BaseType::Bytes,
            };
            type_set.contains(lit_type)
        }
        MatchPattern::Array(_) | MatchPattern::ArrayMin(_) => {
            type_set.contains(crate::types::BaseType::Array)
        }
    }
}

/// Does this pattern fully cover the scrutinee's TypeSet?
/// True when the scrutinee is a single type and the pattern matches that type.
fn pattern_covers_type(type_set: &crate::types::TypeSet, pattern: &MatchPattern) -> bool {
    if !type_set.is_single() {
        return false;
    }
    match pattern {
        MatchPattern::Type(ty) => type_set.contains(*ty),
        MatchPattern::Array(_) | MatchPattern::ArrayMin(_) => {
            type_set.contains(crate::types::BaseType::Array)
        }
        // Literal match doesn't cover the full type (other values possible)
        MatchPattern::Literal(_) => false,
    }
}

/// Warn when an If/While condition is provably not Bool.
///
/// Rill has strict boolean typing — no truthiness. A non-Bool condition
/// always evaluates to false, which is almost certainly a bug.
fn check_condition_types(
    function: &Function,
    types: &type_refinement::TypeAnalysis,
    diagnostics: &mut crate::diagnostics::Diagnostics,
) {
    for block in &function.blocks {
        let (cond_var, span) = match &block.terminator {
            Terminator::If {
                condition, span, ..
            } => (*condition, *span),
            _ => continue,
        };

        let actual = types
            .get_at_exit(block.id, cond_var)
            .copied()
            .unwrap_or(crate::types::TypeSet::all());

        if !actual.contains(crate::types::BaseType::Bool) && !actual.is_empty() {
            diagnostics.warning(
                crate::diagnostics::DiagnosticCode::W009_TypeMismatch,
                span,
                format!(
                    "in function `{}`: condition has type {:?} but Bool required — branch always takes else",
                    function.name, actual,
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Literal, Var};
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

    // ================================================================
    // Dead Match Arm Elimination
    // ================================================================

    #[test]
    fn test_dead_arm_all_pruned() {
        // Match on a UInt with only an Int arm → Jump(default)
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("x".into()),
            TypeSet::uint(),
        )];
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::Match {
                    value: var(0),
                    arms: vec![(MatchPattern::Type(crate::types::BaseType::Int), block(1))],
                    default: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function(blocks, locals);
        let types = type_refinement::analyze_types(&func, None);
        let changes = eliminate_dead_match_arms(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(2)
        ));
    }

    #[test]
    fn test_dead_arm_one_survives_covers() {
        // Match on a UInt with UInt arm + Int arm → Jump(uint_arm)
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("x".into()),
            TypeSet::uint(),
        )];
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::Match {
                    value: var(0),
                    arms: vec![
                        (MatchPattern::Type(crate::types::BaseType::UInt), block(1)),
                        (MatchPattern::Type(crate::types::BaseType::Int), block(2)),
                    ],
                    default: block(3),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
            BasicBlock {
                id: block(3),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function(blocks, locals);
        let types = type_refinement::analyze_types(&func, None);
        let changes = eliminate_dead_match_arms(&mut func, &types);

        assert_eq!(changes, 1);
        // UInt arm covers the scrutinee fully → Jump
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(1)
        ));
    }

    #[test]
    fn test_dead_arm_no_change_when_types_unknown() {
        // Match on a parameter with unknown type → no pruning
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("x".into()),
            TypeSet::all(),
        )];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![],
            terminator: Terminator::Match {
                value: var(0),
                arms: vec![
                    (MatchPattern::Type(crate::types::BaseType::UInt), block(1)),
                    (MatchPattern::Type(crate::types::BaseType::Int), block(2)),
                ],
                default: block(3),
                span: ast::Span::default(),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = type_refinement::analyze_types(&func, None);
        let changes = eliminate_dead_match_arms(&mut func, &types);

        assert_eq!(changes, 0);
    }

    // ================================================================
    // Non-Bool Condition Folding
    // ================================================================

    #[test]
    fn test_non_bool_condition_folded() {
        // if uint_value { } → Jump(else)
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("x".into()),
            TypeSet::uint(),
        )];
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let mut func = make_function(blocks, locals);
        let types = type_refinement::analyze_types(&func, None);
        let changes = fold_non_bool_conditions(&mut func, &types);

        assert_eq!(changes, 1);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(2)
        ));
    }

    #[test]
    fn test_bool_condition_not_folded() {
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("x".into()),
            TypeSet::bool(),
        )];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Const {
                dest: var(0),
                value: Literal::Bool(true),
            })],
            terminator: Terminator::If {
                condition: var(0),
                then_target: block(1),
                else_target: block(2),
                span: ast::Span::default(),
            },
        }];

        let mut func = make_function(blocks, locals);
        let types = type_refinement::analyze_types(&func, None);
        let changes = fold_non_bool_conditions(&mut func, &types);

        assert_eq!(changes, 0);
    }
}
