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

mod cast_elision;
mod coercion;
mod const_fold;
mod definedness;
mod guard_elim;
mod ref_elision;
mod type_refinement;

pub use cast_elision::elide_identity_casts;
pub use coercion::{elide_coercions, insert_coercions};
pub use const_fold::fold_constants;
pub use definedness::{Definedness, DefinednessAnalysis, analyze_definedness, check_definedness};
pub use guard_elim::{eliminate_guards, simplify_cfg};
pub use ref_elision::elide_refs;
pub use type_refinement::{TypeAnalysis, analyze_types};

// Import IR types from parent module
use super::{
    BlockId, CallArg, Function, FunctionRef, Instruction, IntrinsicOp, IrProgram, Terminator, VarId,
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
    for function in &mut program.functions {
        optimize_function(function, builtins, diagnostics);
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

        if folded + refs + coerce + guards + blocks == 0 {
            break;
        }
    }

    // ── Phase 2: Type-informed analysis (on simplified CFG) ────────────

    // Type refinement — intrinsic-aware: Add(UInt, UInt) → {UInt}.
    let types = analyze_types(function, Some(builtins));

    // Type mismatch diagnostics (W009)
    check_intrinsic_types(function, &types, diagnostics);

    // Coercion insertion: makes implicit numeric promotion explicit via Widen.
    // Also replaces provably-incompatible operations with Undefined.
    let coercions = insert_coercions(function, &types);

    // Identity cast/widen elimination: replaces Cast(v, T) and Widen(v, T)
    // with Copy when source type already matches target. Catches user-written
    // redundant casts (e.g. `x as UInt` where x is UInt) and Widens that
    // became identity after type narrowing.
    let cast_elisions = elide_identity_casts(function, &types);

    // If coercion insertion or cast elision changed anything, re-run the
    // Phase 1 fixpoint loop. The expanded IR has:
    //   - Widen(Const(42_u64), 2) → const fold collapses to Const(42_i64)
    //   - Explicit Undefined → definedness sees it, guard elim cleans up
    //   - Identity casts → Copy → const fold may propagate further
    if coercions + cast_elisions > 0 {
        loop {
            let folded = fold_constants(function, builtins, diagnostics);
            let refs = elide_refs(function);
            let coerce = elide_coercions(function);
            let definedness = analyze_definedness(function, Some(builtins));
            let guards = eliminate_guards(function, &definedness);
            let blocks = simplify_cfg(function);
            if folded + refs + coerce + guards + blocks == 0 {
                break;
            }
        }
    }

    // ── Phase 3: Cleanup ───────────────────────────────────────────────

    // Dead code elimination (TODO)
    // eliminate_dead_code(function);
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

#[cfg(test)]
mod tests {
    // Integration tests for the full pipeline will go here
}
