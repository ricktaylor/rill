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
mod cse;
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
pub use cse::eliminate_common_subexpressions;
pub use dce::eliminate_dead_code;
pub use definedness::{
    Definedness, DefinednessAnalysis, analyze_definedness, analyze_definedness_full,
    check_definedness,
};
pub use guard_elim::{eliminate_guards, simplify_cfg};
pub use ref_elision::elide_refs;
pub use type_refinement::{
    ParamDefinedness, ParamTypes, ReturnTypes, TypeAnalysis, analyze_types, infer_return_type,
};

// Import IR types from parent module
use super::{
    BlockId, CallArg, Function, FunctionRef, Instruction, IntrinsicOp, IrProgram, Literal,
    MatchPattern, Terminator, VarId,
};

// Import externs for metadata lookup
use crate::diagnostics::Diagnostics;
use crate::externs::ExternRegistry;
use std::collections::{HashMap, HashSet};

/// Run all optimization passes on a program
pub fn optimize(program: &mut IrProgram, externs: &ExternRegistry, diagnostics: &mut Diagnostics) {
    // Phase A: per-function optimization (intraprocedural)
    for function in &mut program.functions {
        optimize_function(function, externs, diagnostics);
    }

    // Phase M: Function monomorphization
    //
    // When a function is called with different type signatures at different
    // sites, clone it per signature. Each clone gets fully narrowed params
    // in Phase B. Skip recursive functions and limit clones to avoid explosion.
    monomorphize(program, externs);

    // Phase B: interprocedural analysis
    //
    // B1: Argument type + definedness + purity propagation
    // B2: Return type inference — with narrowed params, infer tighter return types.
    // B3: Re-optimize with full interprocedural info.
    //
    // B1 and B2 iterate until stable (handles forward refs, recursion, mutual recursion).

    // B1: Collect argument types, definedness, and purity from call sites
    let (param_types, param_defs) = collect_param_info(program, Some(externs));
    let pure_functions = collect_pure_functions(program, Some(externs));

    // B2: Return type inference (uses narrowed param types)
    let mut return_types = ReturnTypes::new();
    loop {
        let mut changed = false;
        for function in &program.functions {
            let rt = infer_return_type(function, Some(externs), &return_types, &param_types);
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

    // B3: Re-optimize functions with interprocedural type + definedness info.
    // Functions with narrowed params or that call functions with known return types
    // benefit from re-running type-dependent optimizations.
    if !return_types.is_empty() || !param_types.is_empty() {
        for function in &mut program.functions {
            let name = function.name.to_string();
            let has_narrowed_params =
                param_types.contains_key(name.as_str()) || param_defs.contains_key(name.as_str());
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

            if has_narrowed_params || has_user_calls {
                // Re-run Phase 2 with full interprocedural type info
                let types = type_refinement::analyze_types_full(
                    function,
                    Some(externs),
                    &return_types,
                    &param_types,
                );
                let coercions = insert_coercions(function, &types);
                let cast_elisions = elide_identity_casts(function, &types);
                let algebra = simplify_algebra(function, &types);
                let condition_folds = fold_non_bool_conditions(function, &types);
                let dead_arms = eliminate_dead_match_arms(function, &types);

                if coercions + cast_elisions + algebra + condition_folds + dead_arms > 0 {
                    // Use interprocedural param definedness + purity in the fixpoint loop
                    let pd = param_defs.get(name.as_str()).map(|v| v.as_slice());
                    loop {
                        let folded = fold_constants(function, externs, diagnostics);
                        let cse = cse::eliminate_common_subexpressions_with_purity(
                            function,
                            Some(externs),
                            &pure_functions,
                        );
                        let copies = propagate_copies(function);
                        let dead = dce::eliminate_dead_code_with_purity(
                            function,
                            Some(externs),
                            &pure_functions,
                        );
                        let refs = elide_refs(function);
                        let coerce = elide_coercions(function);
                        let definedness = analyze_definedness_full(function, Some(externs), pd);
                        let guards = eliminate_guards(function, &definedness);
                        let blocks = simplify_cfg(function);
                        if folded + cse + copies + dead + refs + coerce + guards + blocks == 0 {
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Infer which user functions are pure (no side effects).
///
/// A function is pure if it contains no SetIndex, WriteRef, or calls to
/// impure functions. Iterates until stable for mutual recursion.
fn collect_pure_functions(
    program: &IrProgram,
    externs: Option<&ExternRegistry>,
) -> HashSet<String> {
    let all_names: HashSet<String> = program
        .functions
        .iter()
        .map(|f| f.name.to_string())
        .collect();

    // Start optimistic: assume all user functions are pure
    let mut pure: HashSet<String> = all_names.clone();

    loop {
        let mut changed = false;
        for function in &program.functions {
            let name = function.name.to_string();
            if !pure.contains(&name) {
                continue; // already marked impure
            }

            let is_pure = function.blocks.iter().all(|block| {
                block.instructions.iter().all(|inst| match &inst.node {
                    // Side effects → impure
                    Instruction::SetIndex { .. } | Instruction::WriteRef { .. } => false,
                    // Call to impure function → impure
                    Instruction::Call {
                        function: func_ref, ..
                    } => {
                        let callee = func_ref.qualified_name();
                        if let Some(registry) = externs
                            && let Some(def) = registry.get(&callee)
                        {
                            return def.meta.purity.is_pure();
                        }
                        // User function: pure only if callee is in our pure set
                        pure.contains(&callee)
                    }
                    _ => true,
                })
            });

            if !is_pure {
                pure.remove(&name);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    pure
}

/// Monomorphize functions called with multiple distinct type signatures.
///
/// For each user function with >1 distinct call-site type signature, clone
/// the function per signature and rewrite callers to target their matching clone.
/// Phase B then narrows each clone's params to its specific signature.
///
/// Limits: max 4 variants per function, skip recursive functions.
fn monomorphize(program: &mut IrProgram, externs: &ExternRegistry) {
    const MAX_VARIANTS: usize = 4;

    // Collect type signatures at each call site: (caller_func_idx, block_idx, inst_idx) → arg types
    #[allow(clippy::type_complexity)]
    let mut call_sites: HashMap<
        String,
        Vec<(usize, BlockId, usize, Vec<crate::types::TypeSet>)>,
    > = HashMap::new();

    // Build function name set for detecting user functions
    let user_functions: HashSet<String> = program
        .functions
        .iter()
        .map(|f| f.name.to_string())
        .collect();

    // Collect call-site signatures using type analysis
    for (func_idx, function) in program.functions.iter().enumerate() {
        let types = analyze_types(function, Some(externs));

        for block in &function.blocks {
            for (inst_idx, inst) in block.instructions.iter().enumerate() {
                if let Instruction::Call {
                    function: func_ref,
                    args,
                    ..
                } = &inst.node
                {
                    let callee_name = func_ref.qualified_name();
                    if !user_functions.contains(&callee_name) {
                        continue; // skip externs
                    }

                    let arg_types: Vec<crate::types::TypeSet> = args
                        .iter()
                        .map(|a| {
                            types
                                .get_at_exit(block.id, a.value)
                                .copied()
                                .unwrap_or(crate::types::TypeSet::all())
                        })
                        .collect();

                    call_sites
                        .entry(callee_name)
                        .or_default()
                        .push((func_idx, block.id, inst_idx, arg_types));
                }
            }
        }
    }

    // Detect recursive functions (call themselves — skip monomorphization)
    let recursive: HashSet<String> = program
        .functions
        .iter()
        .filter(|f| {
            let name = f.name.to_string();
            f.blocks.iter().any(|block| {
                block.instructions.iter().any(|inst| {
                    matches!(&inst.node, Instruction::Call { function: func_ref, .. }
                        if func_ref.qualified_name() == name)
                })
            })
        })
        .map(|f| f.name.to_string())
        .collect();

    // For each function with multiple distinct signatures, create clones
    let mut new_functions: Vec<Function> = Vec::new();
    // Map: (original_name, signature_index) → clone_name
    let mut clone_map: HashMap<(String, usize), String> = HashMap::new();
    // Map: (caller_func_idx, block_id, inst_idx) → clone_name to rewrite
    let mut rewrites: Vec<(usize, BlockId, usize, String)> = Vec::new();

    for (callee_name, sites) in &call_sites {
        if recursive.contains(callee_name) {
            continue;
        }

        // Deduplicate signatures
        let mut unique_sigs: Vec<Vec<crate::types::TypeSet>> = Vec::new();
        let mut site_to_sig: Vec<usize> = Vec::new(); // index into unique_sigs for each site

        for (_, _, _, arg_types) in sites {
            let sig_idx = unique_sigs
                .iter()
                .position(|s| s == arg_types)
                .unwrap_or_else(|| {
                    unique_sigs.push(arg_types.clone());
                    unique_sigs.len() - 1
                });
            site_to_sig.push(sig_idx);
        }

        // Only monomorphize if there are multiple distinct signatures
        if unique_sigs.len() <= 1 || unique_sigs.len() > MAX_VARIANTS {
            continue;
        }

        // Find the original function
        let original = match program
            .functions
            .iter()
            .find(|f| f.name.to_string() == *callee_name)
        {
            Some(f) => f,
            None => continue,
        };

        // Create clones for signatures 1..N (signature 0 keeps the original name)
        for (sig_idx, _sig) in unique_sigs.iter().enumerate().skip(1) {
            let clone_name = format!("{}__mono{}", callee_name, sig_idx);
            let mut clone = original.clone();
            clone.name = crate::ast::Identifier(clone_name.clone());
            clone_map.insert((callee_name.clone(), sig_idx), clone_name);
            new_functions.push(clone);
        }

        // Record which call sites need rewriting (only those targeting non-0 signatures)
        for (site_idx, (func_idx, block_id, inst_idx, _)) in sites.iter().enumerate() {
            let sig_idx = site_to_sig[site_idx];
            if sig_idx > 0
                && let Some(clone_name) = clone_map.get(&(callee_name.clone(), sig_idx))
            {
                rewrites.push((*func_idx, *block_id, *inst_idx, clone_name.clone()));
            }
        }
    }

    if new_functions.is_empty() {
        return; // nothing to monomorphize
    }

    // Add clones to the program
    program.functions.extend(new_functions);

    // Rewrite call sites to target their matching clone
    for (func_idx, block_id, inst_idx, clone_name) in rewrites {
        if func_idx < program.functions.len() {
            let function = &mut program.functions[func_idx];
            if let Some(block) = function.blocks.iter_mut().find(|b| b.id == block_id)
                && inst_idx < block.instructions.len()
                && let Instruction::Call {
                    function: func_ref, ..
                } = &mut block.instructions[inst_idx].node
            {
                func_ref.name = crate::ast::Identifier(clone_name);
                func_ref.namespace = None;
            }
        }
    }

    // Re-optimize the new clones
    let clone_start = program.functions.len() - clone_map.len();
    for function in &mut program.functions[clone_start..] {
        let mut diags = Diagnostics::new();
        optimize_function(function, externs, &mut diags);
    }
}

/// Collect parameter types and definedness from all call sites across the program.
///
/// For each user function, unions the argument TypeSets and meets the argument
/// Definedness from all callers. If all callers pass Defined UInt for param 0,
/// then param 0 narrows to `{UInt}` + `Defined`.
fn collect_param_info(
    program: &IrProgram,
    externs: Option<&ExternRegistry>,
) -> (ParamTypes, ParamDefinedness) {
    // Build function name → param count map
    let func_param_counts: HashMap<String, usize> = program
        .functions
        .iter()
        .map(|f| (f.name.to_string(), f.params.len()))
        .collect();

    let mut param_types = ParamTypes::new();
    let mut param_defs = ParamDefinedness::new();

    for function in &program.functions {
        // Run type + definedness analysis on this function
        let types = analyze_types(function, externs);
        let defs = analyze_definedness(function, externs);

        for block in &function.blocks {
            for inst in &block.instructions {
                if let Instruction::Call {
                    function: func_ref,
                    args,
                    ..
                } = &inst.node
                {
                    let callee_name = func_ref.qualified_name();

                    // Only process calls to user functions
                    let Some(&param_count) = func_param_counts.get(&callee_name) else {
                        continue;
                    };

                    let type_entry = param_types
                        .entry(callee_name.clone())
                        .or_insert_with(|| vec![crate::types::TypeSet::empty(); param_count]);

                    let def_entry = param_defs
                        .entry(callee_name)
                        .or_insert_with(|| vec![Definedness::Defined; param_count]);

                    for (i, arg) in args.iter().enumerate() {
                        if i < param_count {
                            // Union types
                            let arg_type = types
                                .get_at_exit(block.id, arg.value)
                                .copied()
                                .unwrap_or_else(type_refinement::all_types);
                            type_entry[i] = type_entry[i].union(&arg_type);

                            // Meet definedness (most conservative across callers)
                            let arg_def = defs.get_at_exit(block.id, arg.value);
                            def_entry[i] = def_entry[i].meet(arg_def);
                        }
                    }
                }
            }
        }
    }

    (param_types, param_defs)
}

/// Run all optimization passes on a single function
pub fn optimize_function(
    function: &mut Function,
    externs: &ExternRegistry,
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
        let folded = fold_constants(function, externs, diagnostics);
        let cse = eliminate_common_subexpressions(function);
        let copies = propagate_copies(function);
        let dead = eliminate_dead_code(function);
        let refs = elide_refs(function);
        let coerce = elide_coercions(function);

        let definedness = analyze_definedness(function, Some(externs));

        // Emit definedness diagnostics only on the first iteration,
        // before guard elimination reshapes the control flow.
        if first_iteration {
            check_definedness(function, &definedness, Some(externs), diagnostics);
            first_iteration = false;
        }

        let guards = eliminate_guards(function, &definedness);
        let blocks = simplify_cfg(function);

        if folded + cse + copies + dead + refs + coerce + guards + blocks == 0 {
            break;
        }
    }

    // ── Phase 2: Type-informed analysis (on simplified CFG) ────────────

    // Type refinement — intrinsic-aware: Add(UInt, UInt) → {UInt}.
    let types = analyze_types(function, Some(externs));

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
            let folded = fold_constants(function, externs, diagnostics);
            let cse = eliminate_common_subexpressions(function);
            let copies = propagate_copies(function);
            let dead = eliminate_dead_code(function);
            let refs = elide_refs(function);
            let coerce = elide_coercions(function);
            let definedness = analyze_definedness(function, Some(externs));
            let guards = eliminate_guards(function, &definedness);
            let blocks = simplify_cfg(function);
            if folded + cse + copies + dead + refs + coerce + guards + blocks == 0 {
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
                    break;
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

    // ================================================================
    // Interprocedural param propagation
    // ================================================================

    #[test]
    fn test_collect_param_info_single_caller() {
        // fn callee(x) { x + 1 }
        // fn caller() { callee(42) }
        // → callee param x should be {UInt}, Defined
        let callee = Function {
            name: ast::Identifier("callee".into()),
            params: vec![crate::ir::Param {
                var: var(0),
                by_ref: false,
            }],
            rest_param: None,
            locals: vec![
                Var::new(var(0), ast::Identifier("x".into()), TypeSet::all()),
                Var::new(var(1), ast::Identifier("one".into()), TypeSet::uint()),
                Var::new(var(2), ast::Identifier("r".into()), TypeSet::all()),
            ],
            blocks: vec![BasicBlock {
                id: block(0),
                instructions: vec![
                    si(Instruction::Const {
                        dest: var(1),
                        value: Literal::UInt(1),
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
            }],
            entry_block: block(0),
        };

        let caller = Function {
            name: ast::Identifier("caller".into()),
            params: vec![],
            rest_param: None,
            locals: vec![
                Var::new(var(10), ast::Identifier("arg".into()), TypeSet::uint()),
                Var::new(var(11), ast::Identifier("result".into()), TypeSet::all()),
            ],
            blocks: vec![BasicBlock {
                id: block(0),
                instructions: vec![
                    si(Instruction::Const {
                        dest: var(10),
                        value: Literal::UInt(42),
                    }),
                    si(Instruction::Call {
                        dest: var(11),
                        function: crate::ir::FunctionRef {
                            namespace: None,
                            name: ast::Identifier("callee".into()),
                        },
                        args: vec![crate::ir::CallArg {
                            value: var(10),
                            by_ref: false,
                        }],
                    }),
                ],
                terminator: Terminator::Return {
                    value: Some(var(11)),
                },
            }],
            entry_block: block(0),
        };

        let program = IrProgram {
            functions: vec![callee, caller],
            constants: vec![],
            imports: vec![],
        };

        let (param_types, param_defs) = collect_param_info(&program, None);

        // callee's param x should be {UInt}
        let callee_types = param_types.get("callee").unwrap();
        assert_eq!(callee_types.len(), 1);
        assert!(callee_types[0].is_single());
        assert!(callee_types[0].contains(crate::types::BaseType::UInt));

        // callee's param x should be Defined
        let callee_defs = param_defs.get("callee").unwrap();
        assert_eq!(callee_defs.len(), 1);
        assert_eq!(callee_defs[0], Definedness::Defined);
    }
}
