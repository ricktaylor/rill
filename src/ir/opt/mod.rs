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

mod const_fold;
mod definedness;
mod guard_elim;
mod type_refinement;

pub use const_fold::fold_constants;
pub use definedness::analyze_definedness;
pub use guard_elim::{eliminate_guards, simplify_cfg};
pub use type_refinement::analyze_types;

// Import IR types from parent module
use super::{BlockId, CallArg, Function, FunctionRef, Instruction, IrProgram, Terminator, VarId};

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
    // Pass 1: Early constant folding
    // Fold obvious compile-time constants before analysis
    fold_constants(function, builtins, diagnostics);

    // Pass 2: Definedness analysis
    let definedness = analyze_definedness(function, Some(builtins));

    // Pass 2.5: Diagnostics (TODO)
    // emit_diagnostics(function, &definedness);

    // Pass 3: Guard elimination
    let _guards_eliminated = eliminate_guards(function, &definedness);

    // Pass 3.5: CFG simplification
    let _blocks_removed = simplify_cfg(function);

    // Pass 4: Type refinement analysis
    let _types = analyze_types(function, Some(builtins));
    // TODO: Use type analysis for:
    // - Dead arm elimination in Match
    // - Type-specialized code generation

    // Pass 5: Constant folding (cleanup)
    // Fold constants exposed by guard elimination and CFG simplification
    fold_constants(function, builtins, diagnostics);

    // Pass 5.5: Final CFG simplification
    // Cleanup fold may create new Jumps (from folded If/Guard with constant
    // conditions). Merge any new linear chains so the closure compiler sees
    // minimal blocks with no unnecessary NextBlock transitions.
    let _blocks_removed2 = simplify_cfg(function);

    // Pass 6: Dead code elimination (TODO)
    // eliminate_dead_code(function);
}

#[cfg(test)]
mod tests {
    // Integration tests for the full pipeline will go here
}
