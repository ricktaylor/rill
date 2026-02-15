//! Definedness Analysis (Pass 1)
//!
//! Determines which variables are provably defined (not Undefined) at each
//! program point. This enables:
//! - Guard elimination (when value is provably defined/undefined)
//! - Diagnostics (warnings for maybe-undefined, errors for definitely-undefined)
//!
//! The analysis is orthogonal to type analysis - a value can be "definitely
//! defined" without knowing its concrete type.
//!
//! # Intraprocedural by Design
//!
//! Unlike type refinement, this analysis does not need interprocedural tracking.
//! Function parameters are conservatively `MaybeDefined`, and any problematic
//! uses are caught at the use site within the callee. Provenance tracking lets
//! users trace undefined values back to their source (calls, index operations).
//! This is sufficient because the actual error is at the use site, not the call
//! site - passing undefined to a function that ignores the argument is harmless.

use super::{BlockId, CallArg, Function, FunctionRef, Instruction, Terminator, VarId};
use crate::builtins::BuiltinRegistry;
use crate::diagnostics::{DiagnosticCode, Diagnostics};
use std::collections::{HashMap, HashSet, VecDeque};

// ============================================================================
// Definedness Lattice
// ============================================================================

/// Definedness state for a variable
///
/// Lattice ordering: Undefined < MaybeDefined < Defined
/// Meet operation: min (most conservative)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Definedness {
    /// Value is guaranteed to be Undefined
    Undefined,

    /// Value might be Undefined (need runtime check)
    #[default]
    MaybeDefined,

    /// Value is guaranteed to be non-Undefined
    Defined,
}

impl Definedness {
    /// Meet operation: most conservative combination of two states
    ///
    /// Used at Phi nodes and control flow joins
    pub fn meet(self, other: Definedness) -> Definedness {
        use Definedness::*;
        match (self, other) {
            (Defined, Defined) => Defined,
            (Undefined, Undefined) => Undefined,
            _ => MaybeDefined,
        }
    }

    /// Join operation: least upper bound
    ///
    /// Used when combining information from multiple paths
    pub fn join(self, other: Definedness) -> Definedness {
        use Definedness::*;
        match (self, other) {
            (Undefined, Undefined) => Undefined,
            (Defined, Defined) => Defined,
            _ => MaybeDefined,
        }
    }

    /// Check if this state is at least as defined as another
    pub fn at_least_as_defined_as(self, other: Definedness) -> bool {
        use Definedness::*;
        match (self, other) {
            (Defined, _) => true,
            (MaybeDefined, Undefined | MaybeDefined) => true,
            (Undefined, Undefined) => true,
            _ => false,
        }
    }
}

// ============================================================================
// Undefined Provenance
// ============================================================================

/// Source/reason why a variable is undefined or maybe-undefined
#[derive(Debug, Clone)]
pub enum UndefinedSource {
    /// From a call to a function that always returns undefined
    Call { func_name: String },

    /// From an index operation that may fail (out of bounds, key not found)
    Index,

    /// Propagated from another variable (via Copy or Phi)
    Propagated { from: VarId },
}

/// Map from VarId to the source of its undefined-ness
pub type ProvenanceMap = HashMap<VarId, UndefinedSource>;

// ============================================================================
// Analysis State
// ============================================================================

/// Map from (BlockId, VarId) to Definedness at block entry
pub type DefinednessMap = HashMap<(BlockId, VarId), Definedness>;

/// Analysis result for a function
#[derive(Debug)]
pub struct DefinednessAnalysis {
    /// Definedness of each variable at each block's entry point
    pub at_entry: DefinednessMap,

    /// Definedness of each variable at each block's exit point
    pub at_exit: DefinednessMap,

    /// Provenance tracking: why each variable is undefined/maybe-undefined
    pub provenance: ProvenanceMap,
}

impl DefinednessAnalysis {
    /// Get the definedness of a variable at a block's entry
    pub fn get_at_entry(&self, block: BlockId, var: VarId) -> Definedness {
        self.at_entry
            .get(&(block, var))
            .copied()
            .unwrap_or(Definedness::MaybeDefined)
    }

    /// Get the definedness of a variable at a block's exit
    pub fn get_at_exit(&self, block: BlockId, var: VarId) -> Definedness {
        self.at_exit
            .get(&(block, var))
            .copied()
            .unwrap_or(Definedness::MaybeDefined)
    }

    /// Get the provenance (source of undefined-ness) for a variable
    pub fn get_provenance(&self, var: VarId) -> Option<&UndefinedSource> {
        self.provenance.get(&var)
    }
}

// ============================================================================
// CFG Utilities
// ============================================================================

/// Build a map from block ID to block index in the function's block list
fn build_block_index_map(function: &Function) -> HashMap<BlockId, usize> {
    function
        .blocks
        .iter()
        .enumerate()
        .map(|(idx, block)| (block.id, idx))
        .collect()
}

/// Get successor block IDs for a terminator
fn terminator_successors(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Jump { target } => vec![*target],
        Terminator::If {
            then_target,
            else_target,
            ..
        } => vec![*then_target, *else_target],
        Terminator::Match { arms, default, .. } => {
            let mut succs: Vec<_> = arms.iter().map(|(_, target)| *target).collect();
            succs.push(*default);
            succs
        }
        Terminator::Guard {
            defined, undefined, ..
        } => vec![*defined, *undefined],
        Terminator::Return { .. } | Terminator::Exit { .. } | Terminator::Unreachable => vec![],
    }
}

/// Build predecessor map for the CFG
fn build_predecessors(function: &Function) -> HashMap<BlockId, Vec<BlockId>> {
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();

    // Initialize all blocks with empty predecessor lists
    for block in &function.blocks {
        preds.entry(block.id).or_default();
    }

    // Add edges
    for block in &function.blocks {
        for succ in terminator_successors(&block.terminator) {
            preds.entry(succ).or_default().push(block.id);
        }
    }

    preds
}

// ============================================================================
// Transfer Functions
// ============================================================================

/// Compute the definedness of a variable after an instruction
fn transfer_instruction(
    instruction: &Instruction,
    state: &mut HashMap<VarId, Definedness>,
    provenance: &mut ProvenanceMap,
    builtins: Option<&BuiltinRegistry>,
) {
    match instruction {
        // Constants are always defined
        Instruction::Const { dest, .. } => {
            state.insert(*dest, Definedness::Defined);
            // No provenance for defined values
        }

        // Undefined instruction produces Undefined
        Instruction::Undefined { dest } => {
            state.insert(*dest, Definedness::Undefined);
            // Internal undefined - no meaningful provenance to track
        }

        // Copy inherits from source
        Instruction::Copy { dest, src } => {
            let src_def = state.get(src).copied().unwrap_or(Definedness::MaybeDefined);
            state.insert(*dest, src_def);
            // Track propagation if source is not defined
            if src_def != Definedness::Defined {
                provenance.insert(*dest, UndefinedSource::Propagated { from: *src });
            }
        }

        // Index may fail (OOB), so result is MaybeDefined
        Instruction::Index { dest, .. } => {
            state.insert(*dest, Definedness::MaybeDefined);
            provenance.insert(*dest, UndefinedSource::Index);
        }

        // SetIndex doesn't define a new variable
        Instruction::SetIndex { .. } => {}

        // Phi merges from multiple sources
        Instruction::Phi { dest, sources } => {
            let mut first_undefined_source: Option<VarId> = None;
            let result = sources.iter().fold(None, |acc, (_, var)| {
                let var_def = state.get(var).copied().unwrap_or(Definedness::MaybeDefined);
                if var_def != Definedness::Defined && first_undefined_source.is_none() {
                    first_undefined_source = Some(*var);
                }
                match acc {
                    None => Some(var_def),
                    Some(prev) => Some(prev.meet(var_def)),
                }
            });
            let final_def = result.unwrap_or(Definedness::MaybeDefined);
            state.insert(*dest, final_def);
            // Track propagation from first undefined source
            if final_def != Definedness::Defined
                && let Some(src) = first_undefined_source
            {
                provenance.insert(*dest, UndefinedSource::Propagated { from: src });
            }
        }

        // Intrinsic operations on defined values produce defined results
        Instruction::Intrinsic { dest, args, .. } => {
            let all_defined = args
                .iter()
                .all(|arg| state.get(arg).copied() == Some(Definedness::Defined));
            if all_defined {
                state.insert(*dest, Definedness::Defined);
            } else {
                state.insert(*dest, Definedness::MaybeDefined);
                // Find first undefined arg
                if let Some(undefined_arg) = args
                    .iter()
                    .find(|arg| state.get(arg).copied() != Some(Definedness::Defined))
                {
                    provenance.insert(
                        *dest,
                        UndefinedSource::Propagated {
                            from: *undefined_arg,
                        },
                    );
                }
            }
        }

        // Function calls: use builtin metadata if available
        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let definedness = compute_call_definedness(function, args, state, builtins);
            state.insert(*dest, definedness);
            // Track call provenance if result may be undefined
            if definedness != Definedness::Defined {
                provenance.insert(
                    *dest,
                    UndefinedSource::Call {
                        func_name: function.qualified_name(),
                    },
                );
            }
        }

        // MakeRef creates a reference, which is defined if base and key are defined
        Instruction::MakeRef { dest, base, key } => {
            let base_def = state
                .get(base)
                .copied()
                .unwrap_or(Definedness::MaybeDefined);
            let key_def = state.get(key).copied().unwrap_or(Definedness::MaybeDefined);
            // Reference creation itself succeeds, but the referenced slot may be undefined
            // The reference is defined, but dereferencing it may yield undefined
            state.insert(*dest, Definedness::MaybeDefined); // Target may not exist
            // Track provenance from base or key if undefined
            if base_def != Definedness::Defined {
                provenance.insert(*dest, UndefinedSource::Propagated { from: *base });
            } else if key_def != Definedness::Defined {
                provenance.insert(*dest, UndefinedSource::Propagated { from: *key });
            }
        }

        // Drop doesn't produce a value
        Instruction::Drop { .. } => {}
    }
}

/// Compute the definedness of a function call result using builtin metadata
fn compute_call_definedness(
    function: &FunctionRef,
    args: &[CallArg],
    state: &HashMap<VarId, Definedness>,
    builtins: Option<&BuiltinRegistry>,
) -> Definedness {
    let Some(builtin) = builtins.and_then(|r| r.lookup(function)) else {
        // Unknown function or no registry - conservatively assume MaybeDefined
        return Definedness::MaybeDefined;
    };

    // Check if function diverges (never returns)
    if builtin.meta.diverges() {
        // Diverging function - doesn't matter what we return here
        // since control never reaches the destination
        return Definedness::Undefined;
    }

    // Check if function may return undefined (fallible)
    if builtin.meta.purity.may_return_undefined() {
        // Function may return undefined due to:
        // - Domain errors (overflow, div-by-zero, OOB)
        // - External factors (Impure functions)
        return Definedness::MaybeDefined;
    }

    // Function is infallible - returns Defined IF all args are Defined
    let any_undefined = args
        .iter()
        .any(|arg| state.get(&arg.value).copied() == Some(Definedness::Undefined));
    if any_undefined {
        // Undefined input -> undefined output (propagation via Guard)
        return Definedness::MaybeDefined;
    }

    let all_defined = args
        .iter()
        .all(|arg| state.get(&arg.value).copied() == Some(Definedness::Defined));
    if all_defined {
        Definedness::Defined
    } else {
        // Some args are MaybeDefined, so result is MaybeDefined
        Definedness::MaybeDefined
    }
}

/// Apply control flow refinement at a Guard terminator
///
/// In the defined branch, the guarded value is known to be Defined.
/// In the undefined branch, the guarded value is known to be Undefined.
fn apply_guard_refinement(
    terminator: &Terminator,
    state: &HashMap<VarId, Definedness>,
) -> HashMap<BlockId, HashMap<VarId, Definedness>> {
    let mut refined = HashMap::new();

    if let Terminator::Guard {
        value,
        defined,
        undefined,
        ..
    } = terminator
    {
        // Defined branch: value is Defined
        let mut defined_state = state.clone();
        defined_state.insert(*value, Definedness::Defined);
        refined.insert(*defined, defined_state);

        // Undefined branch: value is Undefined
        let mut undefined_state = state.clone();
        undefined_state.insert(*value, Definedness::Undefined);
        refined.insert(*undefined, undefined_state);
    }

    refined
}

// ============================================================================
// Main Analysis
// ============================================================================

/// Analyze definedness for all variables in a function
///
/// Returns a DefinednessAnalysis containing the state at each block's entry
/// and exit points.
///
/// If `builtins` is provided, function call results will use the metadata
/// from the builtin registry to determine definedness more precisely.
pub fn analyze_definedness(
    function: &Function,
    builtins: Option<&BuiltinRegistry>,
) -> DefinednessAnalysis {
    let block_index = build_block_index_map(function);
    let _predecessors = build_predecessors(function);

    // State at entry and exit of each block
    let mut entry_states: HashMap<BlockId, HashMap<VarId, Definedness>> = HashMap::new();
    let mut exit_states: HashMap<BlockId, HashMap<VarId, Definedness>> = HashMap::new();

    // Provenance tracking for undefined values
    let mut provenance: ProvenanceMap = HashMap::new();

    // Initialize entry block with parameter definedness
    let mut initial_state = HashMap::new();
    for param in &function.params {
        // Parameters are MaybeDefined - caller might pass undefined
        initial_state.insert(param.var, Definedness::MaybeDefined);
    }
    if let Some(ref rest_param) = function.rest_param {
        // Rest params collect all remaining args, might include undefined
        initial_state.insert(rest_param.var, Definedness::MaybeDefined);
    }
    entry_states.insert(function.entry_block, initial_state);

    // Worklist algorithm for forward dataflow
    let mut worklist: VecDeque<BlockId> = VecDeque::new();
    worklist.push_back(function.entry_block);

    // Track which blocks have been visited
    let mut in_worklist: HashSet<BlockId> = HashSet::new();
    in_worklist.insert(function.entry_block);

    while let Some(block_id) = worklist.pop_front() {
        in_worklist.remove(&block_id);

        let block_idx = match block_index.get(&block_id) {
            Some(idx) => *idx,
            None => continue, // Block not found (shouldn't happen)
        };
        let block = &function.blocks[block_idx];

        // Get entry state for this block
        let mut state = entry_states.get(&block_id).cloned().unwrap_or_default();

        // Apply transfer function for each instruction
        for spanned_inst in &block.instructions {
            transfer_instruction(&spanned_inst.node, &mut state, &mut provenance, builtins);
        }

        // Check if exit state changed
        let old_exit = exit_states.get(&block_id);
        let changed = old_exit.is_none_or(|old| *old != state);

        if changed {
            exit_states.insert(block_id, state.clone());

            // Apply control flow refinement for Guard terminators
            let refined = apply_guard_refinement(&block.terminator, &state);

            // Propagate to successors
            for succ_id in terminator_successors(&block.terminator) {
                // Compute new entry state for successor
                let new_entry = if let Some(refined_state) = refined.get(&succ_id) {
                    // Use refined state for Guard branches
                    refined_state.clone()
                } else {
                    // Normal case: merge with existing entry state
                    state.clone()
                };

                // Merge with existing entry state from other predecessors
                let entry = entry_states.entry(succ_id).or_default();
                let mut merged_changed = false;

                for (var, def) in &new_entry {
                    let existing = entry.get(var).copied();
                    let merged = match existing {
                        Some(existing_def) => existing_def.meet(*def),
                        None => *def,
                    };
                    if existing != Some(merged) {
                        entry.insert(*var, merged);
                        merged_changed = true;
                    }
                }

                // Also copy over variables not in new_entry
                // (This handles the case where different paths define different variables)

                if merged_changed && !in_worklist.contains(&succ_id) {
                    worklist.push_back(succ_id);
                    in_worklist.insert(succ_id);
                }
            }
        }
    }

    // Convert to the public format
    let mut at_entry = DefinednessMap::new();
    let mut at_exit = DefinednessMap::new();

    for (block_id, state) in entry_states {
        for (var, def) in state {
            at_entry.insert((block_id, var), def);
        }
    }

    for (block_id, state) in exit_states {
        for (var, def) in state {
            at_exit.insert((block_id, var), def);
        }
    }

    DefinednessAnalysis {
        at_entry,
        at_exit,
        provenance,
    }
}

// ============================================================================
// Function Return Analysis
// ============================================================================

/// Metadata about a user-defined function's return behavior
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionReturnInfo {
    /// The definedness of the function's return value
    pub return_definedness: Definedness,
}

/// Map from function name to return info
pub type FunctionReturnMap = HashMap<String, FunctionReturnInfo>;

/// Analyze the return definedness of a function
///
/// Examines all Return terminators and computes the meet of their values'
/// definedness states to determine what the function returns.
pub fn analyze_return_definedness(
    function: &Function,
    analysis: &DefinednessAnalysis,
) -> FunctionReturnInfo {
    let mut return_definedness: Option<Definedness> = None;

    for block in &function.blocks {
        if let Terminator::Return { value } = &block.terminator {
            let this_return = match value {
                Some(var) => {
                    // Get definedness at block exit (after all instructions)
                    analysis
                        .at_exit
                        .get(&(block.id, *var))
                        .copied()
                        .unwrap_or(Definedness::MaybeDefined)
                }
                None => Definedness::Undefined, // No return value = undefined
            };

            return_definedness = Some(match return_definedness {
                None => this_return,
                Some(prev) => prev.meet(this_return),
            });
        }
    }

    FunctionReturnInfo {
        return_definedness: return_definedness.unwrap_or(Definedness::Undefined),
    }
}

/// Analyze return definedness for all functions in a program
pub fn analyze_all_returns(
    functions: &[Function],
    analyses: &HashMap<String, DefinednessAnalysis>,
) -> FunctionReturnMap {
    let mut map = FunctionReturnMap::new();

    for function in functions {
        if let Some(analysis) = analyses.get(&function.name.0) {
            let info = analyze_return_definedness(function, analysis);
            map.insert(function.name.0.clone(), info);
        }
    }

    map
}

// ============================================================================
// Diagnostic Emission
// ============================================================================

/// Check a function for definedness errors and emit diagnostics
///
/// This walks through the IR and reports:
/// - E200: Use of definitely-undefined value (error for control flow, warning for data flow)
/// - E201: Use of maybe-undefined value without guard (warning)
///
/// The function takes the pre-computed analysis results to avoid re-analyzing.
/// If `builtins` is provided, builtin metadata is used for more precise transfer functions.
pub fn check_definedness(
    function: &Function,
    analysis: &DefinednessAnalysis,
    builtins: Option<&BuiltinRegistry>,
    diagnostics: &mut Diagnostics,
) {
    let block_index = build_block_index_map(function);

    for block in &function.blocks {
        // Start with entry state for this block
        let mut state: HashMap<VarId, Definedness> = HashMap::new();

        // Initialize from analysis
        for var in function.locals.iter().map(|v| v.id) {
            if let Some(&def) = analysis.at_entry.get(&(block.id, var)) {
                state.insert(var, def);
            }
        }
        for param in &function.params {
            if let Some(&def) = analysis.at_entry.get(&(block.id, param.var)) {
                state.insert(param.var, def);
            }
        }

        // Check each instruction
        // Note: We use a dummy provenance map here since we already have
        // provenance from the analysis - we just need to track state
        let mut dummy_provenance = ProvenanceMap::new();
        for spanned_inst in &block.instructions {
            check_instruction_uses(
                &spanned_inst.node,
                &state,
                &function.name.0,
                &analysis.provenance,
                spanned_inst.span,
                diagnostics,
            );
            // Apply transfer function to update state for next instruction
            transfer_instruction(
                &spanned_inst.node,
                &mut state,
                &mut dummy_provenance,
                builtins,
            );
        }

        // Check terminator uses
        check_terminator_uses(
            &block.terminator,
            &state,
            &function.name.0,
            &analysis.provenance,
            diagnostics,
        );
    }

    let _ = block_index; // Used for future enhancements
}

/// Check if any operands of an instruction are problematically undefined
fn check_instruction_uses(
    instruction: &Instruction,
    state: &HashMap<VarId, Definedness>,
    func_name: &str,
    provenance: &ProvenanceMap,
    span: crate::ast::Span,
    diagnostics: &mut Diagnostics,
) {
    match instruction {
        // Call: no argument checks needed here
        // - By-ref args might be out params (function writes, doesn't read)
        // - By-value args: if the callee uses the param, that use-site will be checked
        // - Provenance tracking captures where undefined values came from
        Instruction::Call { .. } => {}

        Instruction::Index { base, key, .. } => {
            check_var_use(
                *base,
                state,
                "indexed value",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
            check_var_use(
                *key,
                state,
                "index key",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
        }

        Instruction::SetIndex {
            base, key, value, ..
        } => {
            check_var_use(
                *base,
                state,
                "indexed value",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
            check_var_use(
                *key,
                state,
                "index key",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
            check_var_use(
                *value,
                state,
                "assigned value",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
        }

        Instruction::Intrinsic { args, op, .. } => {
            for (i, arg) in args.iter().enumerate() {
                check_var_use(
                    *arg,
                    state,
                    &format!("argument {} to intrinsic `{:?}`", i + 1, op),
                    func_name,
                    provenance,
                    false, // not control flow
                    span,
                    diagnostics,
                );
            }
        }

        Instruction::MakeRef { base, key, .. } => {
            check_var_use(
                *base,
                state,
                "reference base",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
            check_var_use(
                *key,
                state,
                "reference key",
                func_name,
                provenance,
                false,
                span,
                diagnostics,
            );
        }

        Instruction::Drop { vars } => {
            // Dropping undefined values is fine - no check needed
            let _ = vars;
        }

        // These don't use values in a way that requires definedness
        Instruction::Const { .. }
        | Instruction::Undefined { .. }
        | Instruction::Copy { .. }
        | Instruction::Phi { .. } => {}
    }
}

/// Check if any operands of a terminator are problematically undefined
fn check_terminator_uses(
    terminator: &Terminator,
    state: &HashMap<VarId, Definedness>,
    func_name: &str,
    provenance: &ProvenanceMap,
    diagnostics: &mut Diagnostics,
) {
    match terminator {
        // Return with undefined is valid - it's like returning Unit/void
        // The error is at the call site if the result is used
        Terminator::Return { .. } => {}

        Terminator::If {
            condition, span, ..
        } => {
            // If condition IS control flow - error if undefined
            check_var_use(
                *condition,
                state,
                "if condition",
                func_name,
                provenance,
                true,
                *span,
                diagnostics,
            );
        }

        Terminator::Match { value, span, .. } => {
            // Match scrutinee IS control flow - error if undefined
            check_var_use(
                *value,
                state,
                "match scrutinee",
                func_name,
                provenance,
                true,
                *span,
                diagnostics,
            );
        }

        Terminator::Exit { .. } => {
            // Exit with undefined is valid - embedding application handles Option<ExitCode>
        }

        // Guard explicitly checks definedness - no error to emit
        // (the undefined branch handles the undefined case)
        Terminator::Guard { .. } | Terminator::Jump { .. } | Terminator::Unreachable => {}
    }
}

/// Check a single variable use and emit diagnostic if undefined
///
/// If `is_control_flow` is true, definitely-undefined emits an error (E200).
/// Otherwise, definitely-undefined emits a warning (same as maybe-undefined).
fn check_var_use(
    var: VarId,
    state: &HashMap<VarId, Definedness>,
    context: &str,
    func_name: &str,
    provenance: &ProvenanceMap,
    is_control_flow: bool,
    span: crate::ast::Span,
    diagnostics: &mut Diagnostics,
) {
    let def = state
        .get(&var)
        .copied()
        .unwrap_or(Definedness::MaybeDefined);

    match def {
        Definedness::Undefined => {
            if is_control_flow {
                // Control flow with undefined is an error - fundamentally broken
                emit_undefined_error(var, context, func_name, provenance, span, diagnostics);
            } else {
                // Data flow with undefined is a warning - optimizer will remove dead code
                emit_undefined_warning(var, context, func_name, provenance, span, diagnostics);
            }
        }
        Definedness::MaybeDefined => {
            emit_maybe_undefined_warning(var, context, func_name, provenance, span, diagnostics);
        }
        Definedness::Defined => {
            // All good
        }
    }
}

/// Format provenance information for diagnostics
///
/// Recursively follows `Propagated` links to find the original source of undefined-ness.
fn format_provenance(var: VarId, provenance: &ProvenanceMap) -> Option<String> {
    // Follow the provenance chain to find the root cause
    let (root_source, chain) = trace_provenance(var, provenance);

    root_source.map(|source| {
        let root_msg = match source {
            UndefinedSource::Call { func_name } => {
                format!("value originates from call to `{}`", func_name)
            }
            UndefinedSource::Index => {
                "value originates from index operation that may fail".to_string()
            }
            UndefinedSource::Propagated { .. } => {
                // Shouldn't happen if trace_provenance works correctly
                "value is undefined".to_string()
            }
        };

        if chain.is_empty() {
            root_msg
        } else {
            // Show the propagation chain
            let chain_str: Vec<String> = chain.iter().map(|v| format!("_{}", v.0)).collect();
            format!("{} (via {})", root_msg, chain_str.join(" -> "))
        }
    })
}

/// Trace provenance back to the root source, collecting the propagation chain
///
/// Returns (root_source, chain_of_vars) where chain_of_vars shows the propagation path.
/// Uses a depth limit to prevent infinite loops.
fn trace_provenance(
    var: VarId,
    provenance: &ProvenanceMap,
) -> (Option<&UndefinedSource>, Vec<VarId>) {
    const MAX_DEPTH: usize = 32;
    let mut current = var;
    let mut chain = Vec::new();

    for _ in 0..MAX_DEPTH {
        match provenance.get(&current) {
            Some(UndefinedSource::Propagated { from }) => {
                chain.push(current);
                current = *from;
            }
            Some(source) => {
                // Found the root source (Call or Index)
                return (Some(source), chain);
            }
            None => {
                // No provenance tracked for this var
                if chain.is_empty() {
                    return (None, chain);
                } else {
                    // We have a partial chain but lost the trail
                    return (None, chain);
                }
            }
        }
    }

    // Hit depth limit - return what we have
    (provenance.get(&current), chain)
}

/// Emit an error for use of a definitely-undefined value in control flow
fn emit_undefined_error(
    var: VarId,
    context: &str,
    func_name: &str,
    provenance: &ProvenanceMap,
    span: crate::ast::Span,
    diagnostics: &mut Diagnostics,
) {
    let diag = diagnostics.error(
        DiagnosticCode::E200_UseOfUndefined,
        span,
        format!(
            "use of undefined value `_{}` as {} in function `{}`",
            var.0, context, func_name
        ),
    );
    if let Some(note) = format_provenance(var, provenance) {
        diag.note(span, note);
    }
}

/// Emit a warning for use of a definitely-undefined value in data flow
/// (optimizer will remove dead code, but author should be aware)
fn emit_undefined_warning(
    var: VarId,
    context: &str,
    func_name: &str,
    provenance: &ProvenanceMap,
    span: crate::ast::Span,
    diagnostics: &mut Diagnostics,
) {
    let diag = diagnostics.warning(
        DiagnosticCode::E200_UseOfUndefined,
        span,
        format!(
            "use of undefined value `_{}` as {} in function `{}`",
            var.0, context, func_name
        ),
    );
    if let Some(note) = format_provenance(var, provenance) {
        diag.note(span, note);
    }
}

/// Emit a warning for use of a maybe-undefined value
fn emit_maybe_undefined_warning(
    var: VarId,
    context: &str,
    func_name: &str,
    provenance: &ProvenanceMap,
    span: crate::ast::Span,
    diagnostics: &mut Diagnostics,
) {
    let diag = diagnostics.warning(
        DiagnosticCode::E201_UseOfMaybeUndefined,
        span,
        format!(
            "use of possibly undefined value `_{}` as {} in function `{}`; consider adding a guard",
            var.0, context, func_name
        ),
    );
    if let Some(note) = format_provenance(var, provenance) {
        diag.note(span, note);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Literal, Param, SpannedInst, dummy_span};

    // Helper to create a VarId
    fn var(id: u32) -> VarId {
        VarId(id)
    }

    // Helper to create a BlockId
    fn block(id: u32) -> BlockId {
        BlockId(id)
    }

    // Helper to create a simple identifier
    fn ident(s: &str) -> ast::Identifier {
        ast::Identifier(s.to_string())
    }

    /// Helper to wrap an instruction with a dummy span
    fn si(inst: Instruction) -> SpannedInst {
        ast::Spanned::new(inst, dummy_span())
    }

    /// Build a minimal function with the given blocks
    fn make_function(blocks: Vec<BasicBlock>) -> Function {
        Function {
            name: ident("test"),
            attributes: vec![],
            params: vec![],
            rest_param: None,
            locals: vec![],
            blocks,
            entry_block: block(0),
        }
    }

    /// Build a function with one parameter
    fn make_function_with_param(param_var: VarId, blocks: Vec<BasicBlock>) -> Function {
        Function {
            name: ident("test"),
            attributes: vec![],
            params: vec![Param {
                var: param_var,
                by_ref: false,
            }],
            rest_param: None,
            locals: vec![],
            blocks,
            entry_block: block(0),
        }
    }

    // ========================================================================
    // Basic Tests
    // ========================================================================

    #[test]
    fn test_const_is_defined() {
        // fn test() { let x = 42; }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Const {
                dest: var(0),
                value: Literal::UInt(42),
            })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);

        // x should be Defined at exit
        assert_eq!(analysis.get_at_exit(block(0), var(0)), Definedness::Defined);
    }

    #[test]
    fn test_undefined_is_undefined() {
        // fn test() { let x = undefined; }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Undefined { dest: var(0) })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);

        assert_eq!(
            analysis.get_at_exit(block(0), var(0)),
            Definedness::Undefined
        );
    }

    #[test]
    fn test_param_is_maybe_defined() {
        // fn test(x) { return x; }
        // Params are MaybeDefined because caller might pass undefined
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);

        // Parameter should be MaybeDefined at entry (caller might pass undefined)
        assert_eq!(
            analysis.get_at_entry(block(0), var(0)),
            Definedness::MaybeDefined
        );
    }

    #[test]
    fn test_copy_inherits_definedness() {
        // fn test() { let x = 42; let y = x; }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::Copy {
                    dest: var(1),
                    src: var(0),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);

        assert_eq!(analysis.get_at_exit(block(0), var(0)), Definedness::Defined);
        assert_eq!(analysis.get_at_exit(block(0), var(1)), Definedness::Defined);
    }

    #[test]
    fn test_copy_undefined_stays_undefined() {
        // fn test() { let x = undefined; let y = x; }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Undefined { dest: var(0) }),
                si(Instruction::Copy {
                    dest: var(1),
                    src: var(0),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);

        assert_eq!(
            analysis.get_at_exit(block(0), var(0)),
            Definedness::Undefined
        );
        assert_eq!(
            analysis.get_at_exit(block(0), var(1)),
            Definedness::Undefined
        );
    }

    #[test]
    fn test_index_is_maybe_defined() {
        // fn test(arr) { let x = arr[0]; }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                // Create index key
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(0),
                }),
                // Index into arr
                si(Instruction::Index {
                    dest: var(2),
                    base: var(0),
                    key: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);

        // Index result is MaybeDefined (could be OOB)
        assert_eq!(
            analysis.get_at_exit(block(0), var(2)),
            Definedness::MaybeDefined
        );
    }

    // ========================================================================
    // Control Flow Tests
    // ========================================================================

    #[test]
    fn test_guard_refines_definedness() {
        // fn test(x) {
        //     if let y = x {  // Guard on x
        //         // defined branch: x is Defined
        //     } else {
        //         // undefined branch: x is Undefined
        //     }
        // }
        let blocks = vec![
            // Block 0: Entry with Guard
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::Guard {
                    value: var(0),
                    defined: block(1),
                    undefined: block(2),
                    span: dummy_span(),
                },
            },
            // Block 1: Defined branch
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            // Block 2: Undefined branch
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        // x starts as MaybeDefined (we'll use a call result to simulate this)
        // Actually, let's make x a parameter which is Defined
        // Then create a MaybeDefined value via Index
        let blocks = vec![
            // Block 0: Create maybe-defined value and guard on it
            BasicBlock {
                id: block(0),
                instructions: vec![
                    // x = arr[i] - maybe defined
                    si(Instruction::Index {
                        dest: var(1),
                        base: var(0),
                        key: var(0),
                    }),
                ],
                terminator: Terminator::Guard {
                    value: var(1),
                    defined: block(1),
                    undefined: block(2),
                    span: dummy_span(),
                },
            },
            // Block 1: Defined branch
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
            // Block 2: Undefined branch
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);

        // In defined branch (block 1), var(1) should be Defined
        assert_eq!(
            analysis.get_at_entry(block(1), var(1)),
            Definedness::Defined
        );

        // In undefined branch (block 2), var(1) should be Undefined
        assert_eq!(
            analysis.get_at_entry(block(2), var(1)),
            Definedness::Undefined
        );
    }

    #[test]
    fn test_phi_merges_definedness() {
        // if cond { x = 1 } else { x = undefined }
        // At join: x is MaybeDefined
        let blocks = vec![
            // Block 0: Entry with If
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(2),
                    span: dummy_span(),
                },
            },
            // Block 1: Then branch - x = 1 (Defined)
            BasicBlock {
                id: block(1),
                instructions: vec![si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(1),
                })],
                terminator: Terminator::Jump { target: block(3) },
            },
            // Block 2: Else branch - x = undefined
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Undefined { dest: var(2) })],
                terminator: Terminator::Jump { target: block(3) },
            },
            // Block 3: Join with Phi
            BasicBlock {
                id: block(3),
                instructions: vec![si(Instruction::Phi {
                    dest: var(3),
                    sources: vec![(block(1), var(1)), (block(2), var(2))],
                })],
                terminator: Terminator::Return {
                    value: Some(var(3)),
                },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);

        // var(1) in block 1 exit is Defined
        assert_eq!(analysis.get_at_exit(block(1), var(1)), Definedness::Defined);

        // var(2) in block 2 exit is Undefined
        assert_eq!(
            analysis.get_at_exit(block(2), var(2)),
            Definedness::Undefined
        );

        // var(3) after Phi should be MaybeDefined (meet of Defined and Undefined)
        assert_eq!(
            analysis.get_at_exit(block(3), var(3)),
            Definedness::MaybeDefined
        );
    }

    #[test]
    fn test_phi_all_defined_stays_defined() {
        // if cond { x = 1 } else { x = 2 }
        // At join: x is Defined
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(2),
                    span: dummy_span(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(1),
                })],
                terminator: Terminator::Jump { target: block(3) },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Const {
                    dest: var(2),
                    value: Literal::UInt(2),
                })],
                terminator: Terminator::Jump { target: block(3) },
            },
            BasicBlock {
                id: block(3),
                instructions: vec![si(Instruction::Phi {
                    dest: var(3),
                    sources: vec![(block(1), var(1)), (block(2), var(2))],
                })],
                terminator: Terminator::Return {
                    value: Some(var(3)),
                },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);

        // Both sources are Defined, so Phi result should be Defined
        assert_eq!(analysis.get_at_exit(block(3), var(3)), Definedness::Defined);
    }

    // ========================================================================
    // Loop Tests
    // ========================================================================

    #[test]
    fn test_loop_with_maybe_defined() {
        // while true { x = arr[i]; }
        // x may or may not be defined depending on iteration
        let blocks = vec![
            // Block 0: Entry
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::Jump { target: block(1) },
            },
            // Block 1: Loop header
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(2),
                    else_target: block(3),
                    span: dummy_span(),
                },
            },
            // Block 2: Loop body
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Index {
                    dest: var(1),
                    base: var(0),
                    key: var(0),
                })],
                terminator: Terminator::Jump { target: block(1) },
            },
            // Block 3: Exit
            BasicBlock {
                id: block(3),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);

        // x (var 1) in loop body should be MaybeDefined (Index result)
        assert_eq!(
            analysis.get_at_exit(block(2), var(1)),
            Definedness::MaybeDefined
        );
    }

    // ========================================================================
    // Lattice Tests
    // ========================================================================

    #[test]
    fn test_lattice_meet() {
        use Definedness::*;

        assert_eq!(Defined.meet(Defined), Defined);
        assert_eq!(Undefined.meet(Undefined), Undefined);
        assert_eq!(Defined.meet(Undefined), MaybeDefined);
        assert_eq!(Undefined.meet(Defined), MaybeDefined);
        assert_eq!(MaybeDefined.meet(Defined), MaybeDefined);
        assert_eq!(Defined.meet(MaybeDefined), MaybeDefined);
        assert_eq!(MaybeDefined.meet(Undefined), MaybeDefined);
        assert_eq!(MaybeDefined.meet(MaybeDefined), MaybeDefined);
    }

    #[test]
    fn test_lattice_ordering() {
        use Definedness::*;

        // Defined is at least as defined as everything
        assert!(Defined.at_least_as_defined_as(Defined));
        assert!(Defined.at_least_as_defined_as(MaybeDefined));
        assert!(Defined.at_least_as_defined_as(Undefined));

        // MaybeDefined is at least as defined as itself and Undefined
        assert!(MaybeDefined.at_least_as_defined_as(MaybeDefined));
        assert!(MaybeDefined.at_least_as_defined_as(Undefined));
        assert!(!MaybeDefined.at_least_as_defined_as(Defined));

        // Undefined is only at least as defined as itself
        assert!(Undefined.at_least_as_defined_as(Undefined));
        assert!(!Undefined.at_least_as_defined_as(MaybeDefined));
        assert!(!Undefined.at_least_as_defined_as(Defined));
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_empty_function() {
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![],
            terminator: Terminator::Return { value: None },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);

        // Should not panic, entry/exit should be empty
        assert!(analysis.at_entry.is_empty() || analysis.at_exit.is_empty());
    }

    #[test]
    fn test_call_result_is_maybe_defined() {
        // fn test() { let x = some_call(); }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Call {
                dest: var(0),
                function: FunctionRef {
                    namespace: None,
                    name: ident("some_fn"),
                },
                args: vec![],
            })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);

        // Call results are conservatively MaybeDefined
        assert_eq!(
            analysis.get_at_exit(block(0), var(0)),
            Definedness::MaybeDefined
        );
    }

    // ========================================================================
    // Builtin Registry Tests
    // ========================================================================

    #[test]
    fn test_call_with_always_defined_return() {
        use crate::builtins::standard_builtins;

        // core.make_array returns Array (not optional) - always defined
        let registry = standard_builtins();

        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                // Create some constant args
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(2),
                }),
                // Call core.make_array with defined args
                si(Instruction::Call {
                    dest: var(2),
                    function: FunctionRef {
                        namespace: None,
                        name: ident("core::make_array"),
                    },
                    args: vec![
                        CallArg {
                            value: var(0),
                            by_ref: false,
                        },
                        CallArg {
                            value: var(1),
                            by_ref: false,
                        },
                    ],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let func = make_function(blocks);

        // Without registry - conservatively MaybeDefined
        let analysis_no_reg = analyze_definedness(&func, None);
        assert_eq!(
            analysis_no_reg.get_at_exit(block(0), var(2)),
            Definedness::MaybeDefined
        );

        // With registry - Defined (core.make_array is infallible)
        let analysis_with_reg = analyze_definedness(&func, Some(&registry));
        assert_eq!(
            analysis_with_reg.get_at_exit(block(0), var(2)),
            Definedness::Defined
        );
    }

    #[test]
    fn test_call_with_fallible_return() {
        use crate::builtins::standard_builtins;

        // core.add is fallible (overflow possible) - may return undefined
        let registry = standard_builtins();

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
                si(Instruction::Call {
                    dest: var(2),
                    function: FunctionRef {
                        namespace: None,
                        name: ident("core::add"),
                    },
                    args: vec![
                        CallArg {
                            value: var(0),
                            by_ref: false,
                        },
                        CallArg {
                            value: var(1),
                            by_ref: false,
                        },
                    ],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let func = make_function(blocks);

        // Even with registry, core.add is fallible (may overflow), so MaybeDefined
        let analysis = analyze_definedness(&func, Some(&registry));
        assert_eq!(
            analysis.get_at_exit(block(0), var(2)),
            Definedness::MaybeDefined
        );
    }

    #[test]
    fn test_call_unknown_function() {
        use crate::builtins::standard_builtins;

        let registry = standard_builtins();

        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Call {
                dest: var(0),
                function: FunctionRef {
                    namespace: None,
                    name: ident("unknown_function"),
                },
                args: vec![],
            })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);

        // Unknown function - conservatively MaybeDefined even with registry
        let analysis = analyze_definedness(&func, Some(&registry));
        assert_eq!(
            analysis.get_at_exit(block(0), var(0)),
            Definedness::MaybeDefined
        );
    }

    // ========================================================================
    // Diagnostic Emission Tests
    // ========================================================================

    #[test]
    fn test_check_undefined_use_emits_error() {
        let mut diagnostics = Diagnostics::new();

        // fn test() { let x = undefined; if x { } }
        // Using undefined as a condition is an error
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Undefined { dest: var(0) })],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(1),
                    span: dummy_span(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);
        check_definedness(&func, &analysis, None, &mut diagnostics);

        // Should have an error for using undefined value as condition
        assert!(diagnostics.has_errors());
        assert_eq!(diagnostics.error_count(), 1);
    }

    #[test]
    fn test_check_maybe_undefined_emits_warning() {
        let mut diagnostics = Diagnostics::new();

        // fn test(x) { if x { } }
        // x is MaybeDefined (parameter), using as condition emits warning
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(1),
                    span: dummy_span(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);
        check_definedness(&func, &analysis, None, &mut diagnostics);

        // Should have a warning for using maybe-undefined value as condition
        assert!(!diagnostics.has_errors()); // Warnings don't count as errors
        assert!(diagnostics.warning_count() >= 1);
    }

    #[test]
    fn test_check_defined_use_no_diagnostic() {
        let mut diagnostics = Diagnostics::new();

        // fn test() { let x = 42; return x; }
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Const {
                dest: var(0),
                value: Literal::UInt(42),
            })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);
        check_definedness(&func, &analysis, None, &mut diagnostics);

        // No diagnostics for defined values
        assert!(!diagnostics.has_errors());
        assert_eq!(diagnostics.warning_count(), 0);
    }

    #[test]
    fn test_check_guarded_use_no_error() {
        let mut diagnostics = Diagnostics::new();

        // fn test(x) {
        //     if let y = x {  // Guard on x
        //         return y;   // y is Defined here
        //     }
        // }
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![
                    // Create maybe-defined value
                    si(Instruction::Index {
                        dest: var(1),
                        base: var(0),
                        key: var(0),
                    }),
                ],
                terminator: Terminator::Guard {
                    value: var(1),
                    defined: block(1),
                    undefined: block(2),
                    span: dummy_span(),
                },
            },
            // Defined branch - var(1) is Defined here
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
            // Undefined branch
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_definedness(&func, None);
        check_definedness(&func, &analysis, None, &mut diagnostics);

        // In the defined branch, var(1) is Defined, so no error
        // The only warnings should be for the initial Index operation
        assert!(!diagnostics.has_errors());
    }

    #[test]
    fn test_check_undefined_call_args_no_warning() {
        let mut diagnostics = Diagnostics::new();

        // fn test() { let x = undefined; foo(x, &x); }
        // No warning at call site - the callee's use of the param will be checked
        // By-ref could be out param, by-value defers to callee's use-site
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Undefined { dest: var(0) }),
                si(Instruction::Call {
                    dest: var(1),
                    function: FunctionRef {
                        namespace: None,
                        name: ident("foo"),
                    },
                    args: vec![
                        CallArg {
                            value: var(0),
                            by_ref: false, // by-value
                        },
                        CallArg {
                            value: var(0),
                            by_ref: true, // by-ref
                        },
                    ],
                }),
            ],
            terminator: Terminator::Return { value: None },
        }];

        let func = make_function(blocks);
        let analysis = analyze_definedness(&func, None);
        check_definedness(&func, &analysis, None, &mut diagnostics);

        // No warnings for call arguments - deferred to callee's use-site
        assert!(!diagnostics.has_errors());
        assert_eq!(diagnostics.warning_count(), 0);
    }

    // ========================================================================
    // Provenance Tracking Tests
    // ========================================================================

    #[test]
    fn test_provenance_direct_call() {
        // Test that provenance correctly tracks a direct call
        let mut provenance = ProvenanceMap::new();
        provenance.insert(
            var(0),
            UndefinedSource::Call {
                func_name: "get_value".to_string(),
            },
        );

        let msg = format_provenance(var(0), &provenance);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("call to `get_value`"));
    }

    #[test]
    fn test_provenance_direct_index() {
        // Test that provenance correctly tracks an index operation
        let mut provenance = ProvenanceMap::new();
        provenance.insert(var(0), UndefinedSource::Index);

        let msg = format_provenance(var(0), &provenance);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("index operation"));
    }

    #[test]
    fn test_provenance_single_propagation() {
        // v0 = call(), v1 = copy v0
        let mut provenance = ProvenanceMap::new();
        provenance.insert(
            var(0),
            UndefinedSource::Call {
                func_name: "source_fn".to_string(),
            },
        );
        provenance.insert(var(1), UndefinedSource::Propagated { from: var(0) });

        let msg = format_provenance(var(1), &provenance);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        // Should trace back to the call
        assert!(msg.contains("call to `source_fn`"));
        // Should show the propagation chain
        assert!(msg.contains("via"));
        assert!(msg.contains("_1"));
    }

    #[test]
    fn test_provenance_chain_propagation() {
        // v0 = call(), v1 = copy v0, v2 = copy v1, v3 = copy v2
        let mut provenance = ProvenanceMap::new();
        provenance.insert(
            var(0),
            UndefinedSource::Call {
                func_name: "root_call".to_string(),
            },
        );
        provenance.insert(var(1), UndefinedSource::Propagated { from: var(0) });
        provenance.insert(var(2), UndefinedSource::Propagated { from: var(1) });
        provenance.insert(var(3), UndefinedSource::Propagated { from: var(2) });

        let msg = format_provenance(var(3), &provenance);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        // Should trace all the way back to root_call
        assert!(msg.contains("call to `root_call`"));
        // Should show the chain
        assert!(msg.contains("via"));
        assert!(msg.contains("_3"));
        assert!(msg.contains("_2"));
        assert!(msg.contains("_1"));
    }

    #[test]
    fn test_provenance_index_propagation() {
        // v0 = arr[i], v1 = copy v0
        let mut provenance = ProvenanceMap::new();
        provenance.insert(var(0), UndefinedSource::Index);
        provenance.insert(var(1), UndefinedSource::Propagated { from: var(0) });

        let msg = format_provenance(var(1), &provenance);
        assert!(msg.is_some());
        let msg = msg.unwrap();
        // Should trace back to the index
        assert!(msg.contains("index operation"));
        assert!(msg.contains("via"));
    }
}
