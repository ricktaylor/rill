//! Type Refinement Analysis (Pass 3)
//!
//! Narrows TypeSets for variables based on control flow. This enables:
//! - Dead arm elimination in Match (when type is impossible)
//! - Type-specialized code generation
//! - Better optimization of type-specific operations
//!
//! Key refinement points:
//! - Match terminators: each arm knows the matched type
//! - Guard terminators: defined branch knows value is not undefined
//! - Const instructions: produce known single types
//! - Call instructions: use extern metadata for return types

use crate::externs::ExternRegistry;
use crate::ir::{BlockId, Function, FunctionRef, Instruction, Terminator, VarId};
use crate::types::{BaseType, TypeSet};
use std::collections::{HashMap, HashSet, VecDeque};

// ============================================================================
// Analysis State
// ============================================================================

/// Analysis result for a function — one TypeSet per VarId.
///
/// In SSA form, each VarId is defined exactly once, so its type is
/// determined at its definition site. Type narrowing after Match arms
/// is handled by narrowing copies (pi-nodes) that create new VarIds.
#[derive(Debug)]
pub struct TypeAnalysis {
    types: HashMap<VarId, TypeSet>,
}

impl TypeAnalysis {
    /// Get the TypeSet of a variable.
    pub fn get(&self, var: VarId) -> Option<&TypeSet> {
        self.types.get(&var)
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

// ============================================================================
// Transfer Functions
// ============================================================================

/// Compute the TypeSet of a variable after an instruction
fn transfer_instruction(
    instruction: &Instruction,
    state: &mut HashMap<VarId, TypeSet>,
    externs: Option<&ExternRegistry>,
    return_types: &ReturnTypes,
    declared_types: &HashMap<VarId, TypeSet>,
) {
    match instruction {
        // Undefined produces only undefined (no concrete types)
        Instruction::Const {
            dest,
            value: crate::ir::Literal::Undefined,
        } => {
            state.insert(*dest, TypeSet::undefined());
        }

        // Constants have known single types
        Instruction::Const { dest, value } => {
            let ty = match value {
                crate::ir::Literal::Bool(_) => BaseType::Bool,
                crate::ir::Literal::UInt(_) => BaseType::UInt,
                crate::ir::Literal::Int(_) => BaseType::Int,
                crate::ir::Literal::Float(_) => BaseType::Float,
                crate::ir::Literal::Text(_) => BaseType::Text,
                crate::ir::Literal::Bytes(_) => BaseType::Bytes,
                crate::ir::Literal::Undefined => unreachable!(),
            };
            state.insert(*dest, TypeSet::single(ty));
        }

        // Copy inherits the source type, constrained by the dest's declared type.
        // Narrowing copies (from emit_guard/emit_match) have a narrower declared
        // type — the intersection gives the correct narrowed result.
        Instruction::Copy { dest, src } => {
            let src_type = state.get(src).copied().unwrap_or_else(all_types);
            let declared = declared_types.get(dest).copied().unwrap_or_else(all_types);
            state.insert(*dest, src_type.intersection(&declared));
        }

        // Index result: element type of base
        // Note: definedness (OOB) is tracked by definedness analysis, not here
        Instruction::Index { dest, base, .. } => {
            if let Some(base_type) = state.get(base)
                && base_type.is_single()
                && (base_type.contains(BaseType::Text) || base_type.contains(BaseType::Bytes))
            {
                // text[i] and bytes[i] both return UInt (code point / byte value)
                state.insert(*dest, TypeSet::single(BaseType::UInt));
            } else {
                // Array/Map/unknown: result could be any type
                state.insert(*dest, all_types());
            }
        }

        // SetIndex doesn't produce a value
        Instruction::SetIndex { .. } => {}

        // Phi: union of all incoming types
        Instruction::Phi { dest, sources } => {
            let result = sources.iter().fold(None, |acc: Option<TypeSet>, (_, var)| {
                let var_type = state.get(var).cloned().unwrap_or_else(all_types);
                match acc {
                    None => Some(var_type),
                    Some(prev) => Some(prev.union(&var_type)),
                }
            });
            state.insert(*dest, result.unwrap_or_else(all_types));
        }

        // Intrinsic: refine result type based on operand types.
        // Fallibility (Undefined from domain errors) is included in result_type().
        // Expression-level guards prevent Undefined cascade at intrinsic inputs.
        Instruction::Intrinsic { dest, op, args } => {
            let arg_types: Vec<TypeSet> = args
                .iter()
                .map(|v| state.get(v).cloned().unwrap_or_else(all_types))
                .collect();
            state.insert(*dest, op.result_type_refined(&arg_types));
        }

        // Call: use extern metadata if available
        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let type_set = compute_call_type(function, args, state, externs, return_types);
            state.insert(*dest, type_set);
        }

        // MakeRef: element ref reads base[key], same type rules as Index.
        // Whole-value ref has the same type as its base.
        Instruction::MakeRef { dest, base, key } => {
            if key.is_some() {
                // Element ref: same type narrowing as Index
                if let Some(base_type) = state.get(base)
                    && base_type.is_single()
                    && (base_type.contains(BaseType::Text) || base_type.contains(BaseType::Bytes))
                {
                    // text[i] and bytes[i] both return UInt (code point / byte value)
                    state.insert(*dest, TypeSet::single(BaseType::UInt));
                    return;
                }
                state.insert(*dest, all_types());
            } else {
                // Whole-value ref: same type as base
                if let Some(base_type) = state.get(base) {
                    state.insert(*dest, *base_type);
                } else {
                    state.insert(*dest, all_types());
                }
            }
        }

        // WriteRef: side effect only (writes through a reference), no dest
        Instruction::WriteRef { .. } => {}

        // Append: mutates array, result is Array type
        Instruction::Append { dest, .. } => {
            state.insert(*dest, TypeSet::single(BaseType::Array));
        }

        Instruction::Assign { .. } | Instruction::Read { .. } => {
            unreachable!("pre-SSA instruction; removed by mem2reg")
        }
    }
}

/// Compute the return type of a function call using extern metadata
fn compute_call_type(
    function: &FunctionRef,
    _args: &[VarId],
    _state: &HashMap<VarId, TypeSet>,
    externs: Option<&ExternRegistry>,
    return_types: &ReturnTypes,
) -> TypeSet {
    let Some(def) = externs.and_then(|r| r.lookup(function)) else {
        // Not an extern — check inferred return types for user functions
        let name = function.qualified_name();
        if let Some(rt) = return_types.get(&name)
            && !rt.is_empty()
        {
            return *rt;
        }
        // Not an extern and no inferred return type yet.
        // During Phase A (per-function): return types haven't been collected.
        // During Phase B (interprocedural): recursive calls or functions
        // analyzed later in the iteration — conservatively return all types.
        // Truly undefined functions are caught by the link phase (E500).
        return all_types();
    };

    // If the function diverges, it never returns (empty type set)
    if def.meta.diverges() {
        return TypeSet::none();
    }

    // Get the return type signature and convert to TypeSet
    // Note: fallibility (may_return_undefined) is tracked by Definedness analysis
    type_sig_to_type_set(def.meta.returns.type_sig())
}

/// Convert an extern's TypeSet to analysis TypeSet
/// (Now they're the same type, so just clone)
fn type_sig_to_type_set(sig: &TypeSet) -> TypeSet {
    if sig.is_empty() {
        // Empty types means any type
        all_types()
    } else {
        *sig
    }
}

/// Apply type refinement at a Match terminator
///
/// In each arm, the matched value is known to have the matched type.
fn apply_match_refinement(
    terminator: &Terminator,
    state: &HashMap<VarId, TypeSet>,
) -> HashMap<BlockId, HashMap<VarId, TypeSet>> {
    let mut refined: HashMap<BlockId, HashMap<VarId, TypeSet>> = HashMap::new();

    if let Terminator::Match {
        value,
        arms,
        default,
        ..
    } = terminator
    {
        // Get the current type of the value
        let current_type = state.get(value).cloned().unwrap_or_else(all_types);

        // For each arm, refine to the matched type
        for (pattern, target) in arms {
            let mut arm_state = state.clone();
            let refined_type = match pattern {
                crate::ir::MatchPattern::Type(ty) => TypeSet::single(*ty),
                crate::ir::MatchPattern::Literal(lit) => {
                    let ty = match lit {
                        crate::ir::Literal::Bool(_) => BaseType::Bool,
                        crate::ir::Literal::UInt(_) => BaseType::UInt,
                        crate::ir::Literal::Int(_) => BaseType::Int,
                        crate::ir::Literal::Float(_) => BaseType::Float,
                        crate::ir::Literal::Text(_) => BaseType::Text,
                        crate::ir::Literal::Bytes(_) => BaseType::Bytes,
                        crate::ir::Literal::Undefined => BaseType::Undefined,
                    };
                    TypeSet::single(ty)
                }
                crate::ir::MatchPattern::Array(_) | crate::ir::MatchPattern::ArrayMin(_) => {
                    TypeSet::single(BaseType::Array)
                }
            };
            arm_state.insert(*value, refined_type);
            // If multiple arms target the same block, union the refined types
            if let Some(existing) = refined.get_mut(target) {
                for (var, new_type) in &arm_state {
                    let entry = existing.entry(*var).or_insert(TypeSet::none());
                    *entry = entry.union(new_type);
                }
            } else {
                refined.insert(*target, arm_state);
            }
        }

        // For default arm, exclude the matched types
        let mut default_state = state.clone();
        let mut remaining = current_type;
        for (pattern, _) in arms {
            match pattern {
                crate::ir::MatchPattern::Type(ty) => {
                    remaining = remaining.difference(&TypeSet::single(*ty));
                }
                crate::ir::MatchPattern::Literal(_) => {
                    // Don't remove type for literal match - value could be different literal
                }
                crate::ir::MatchPattern::Array(_) | crate::ir::MatchPattern::ArrayMin(_) => {
                    // Array patterns match arrays - but there could be arrays of other lengths
                }
            }
        }
        default_state.insert(*value, remaining);
        refined.insert(*default, default_state);
    }

    refined
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a TypeSet containing all base types
pub(super) fn all_types() -> TypeSet {
    TypeSet::any()
}

// ============================================================================
// Main Analysis
// ============================================================================

/// Inferred return types for user-defined functions.
pub type ReturnTypes = HashMap<String, TypeSet>;

/// Inferred parameter types for user-defined functions.
/// Maps function name → Vec of TypeSets (one per parameter, positional).
pub type ParamTypes = HashMap<String, Vec<TypeSet>>;

/// Infer the return type of a function from its Return terminators.
///
/// Runs type analysis, then unions the TypeSets of all Return values.
/// Functions with no return value (all returns are `None`) produce `TypeSet::none()`.
pub fn infer_return_type(
    function: &Function,
    externs: Option<&ExternRegistry>,
    return_types: &ReturnTypes,
    param_types: &ParamTypes,
) -> TypeSet {
    let types = analyze_types_full(function, externs, return_types, param_types);

    let mut result = TypeSet::none();
    for block in &function.blocks {
        if let Terminator::Return { value: Some(v) } = &block.terminator
            && let Some(ts) = types.get(*v)
        {
            result = result.union(ts);
        }
    }
    result
}

/// Analyze types for all variables in a function
///
/// Returns a TypeAnalysis containing the TypeSet at each block's entry
/// and exit points.
pub fn analyze_types(function: &Function, externs: Option<&ExternRegistry>) -> TypeAnalysis {
    analyze_types_full(function, externs, &ReturnTypes::new(), &ParamTypes::new())
}

/// Analyze types with full interprocedural information.
pub fn analyze_types_full(
    function: &Function,
    externs: Option<&ExternRegistry>,
    return_types: &ReturnTypes,
    param_types: &ParamTypes,
) -> TypeAnalysis {
    let block_index = build_block_index_map(function);

    // Declared types from locals — used to constrain narrowing copies
    let declared_types: HashMap<VarId, TypeSet> =
        function.locals.iter().map(|v| (v.id, v.type_set)).collect();

    // State at entry and exit of each block
    let mut entry_states: HashMap<BlockId, HashMap<VarId, TypeSet>> = HashMap::new();
    let mut exit_states: HashMap<BlockId, HashMap<VarId, TypeSet>> = HashMap::new();

    // Initialize entry block with parameter types
    let mut initial_state = HashMap::new();
    let propagated = param_types.get(function.name.0.as_str());
    for (i, param) in function.params.iter().enumerate() {
        // Use propagated type from call sites if available, else all types
        let ty = propagated
            .and_then(|pts| pts.get(i).copied())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(all_types);
        initial_state.insert(param.var, ty);
    }
    if let Some(ref rest_param) = function.rest_param {
        // Rest param is always an array
        initial_state.insert(rest_param.var, TypeSet::single(BaseType::Array));
    }
    entry_states.insert(function.entry_block, initial_state);

    // Worklist algorithm for forward dataflow
    let mut worklist: VecDeque<BlockId> = VecDeque::new();
    worklist.push_back(function.entry_block);

    let mut in_worklist: HashSet<BlockId> = HashSet::new();
    in_worklist.insert(function.entry_block);

    while let Some(block_id) = worklist.pop_front() {
        in_worklist.remove(&block_id);

        let block_idx = match block_index.get(&block_id) {
            Some(idx) => *idx,
            None => continue,
        };
        let block = &function.blocks[block_idx];

        // Get entry state for this block
        let mut state = entry_states.get(&block_id).cloned().unwrap_or_default();

        // Apply transfer function for each instruction
        for spanned_inst in &block.instructions {
            transfer_instruction(
                &spanned_inst.node,
                &mut state,
                externs,
                return_types,
                &declared_types,
            );
        }

        // Check if exit state changed
        let old_exit = exit_states.get(&block_id);
        let changed = old_exit.is_none_or(|old| *old != state);

        if changed {
            exit_states.insert(block_id, state.clone());

            // Apply control flow refinement
            let match_refined = apply_match_refinement(&block.terminator, &state);

            // Propagate to successors
            for succ_id in block.terminator.successors() {
                // Compute new entry state for successor
                let new_entry = match_refined
                    .get(&succ_id)
                    .cloned()
                    .unwrap_or_else(|| state.clone());

                // Merge with existing entry state from other predecessors
                let entry = entry_states.entry(succ_id).or_default();
                let mut merged_changed = false;

                for (var, new_type) in &new_entry {
                    let existing = entry.get(var);
                    let merged = match existing {
                        Some(existing_type) => existing_type.union(new_type),
                        None => *new_type,
                    };
                    if existing != Some(&merged) {
                        entry.insert(*var, merged);
                        merged_changed = true;
                    }
                }

                if merged_changed && !in_worklist.contains(&succ_id) {
                    worklist.push_back(succ_id);
                    in_worklist.insert(succ_id);
                }
            }
        }
    }

    // Build per-VarId types from the defining block's exit state.
    //
    // In SSA, each VarId is defined exactly once. The type is determined
    // by the instruction that defines it, not by which block uses it.
    // Match refinement may propagate narrowed types for a VarId into
    // successor blocks' entry states, but those are refinements of the
    // original — the definition-site type is the canonical one.
    //
    // We use the exit state of the entry block for params, and for each
    // instruction's dest, we use the exit state of the block it's in.
    let mut types: HashMap<VarId, TypeSet> = HashMap::new();

    // Params: from entry block's exit state
    if let Some(entry_exit) = exit_states.get(&function.entry_block) {
        for param in &function.params {
            if let Some(&ts) = entry_exit.get(&param.var) {
                types.insert(param.var, ts);
            }
        }
        if let Some(ref rest) = function.rest_param
            && let Some(&ts) = entry_exit.get(&rest.var)
        {
            types.insert(rest.var, ts);
        }
    }

    // Instructions: from their defining block's exit state
    for block in &function.blocks {
        if let Some(exit) = exit_states.get(&block.id) {
            for inst in &block.instructions {
                // Get the dest VarId if this instruction defines one
                let dest = match &inst.node {
                    Instruction::Const { dest, .. }
                    | Instruction::Copy { dest, .. }
                    | Instruction::Index { dest, .. }
                    | Instruction::Intrinsic { dest, .. }
                    | Instruction::Call { dest, .. }
                    | Instruction::MakeRef { dest, .. }
                    | Instruction::Append { dest, .. }
                    | Instruction::Phi { dest, .. }
                    | Instruction::Read { dest, .. } => Some(*dest),
                    Instruction::SetIndex { .. }
                    | Instruction::WriteRef { .. }
                    | Instruction::Assign { .. } => None,
                };

                if let Some(var) = dest
                    && let Some(&ts) = exit.get(&var)
                {
                    types.insert(var, ts);
                }
            }
        }
    }

    TypeAnalysis { types }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Literal, MatchPattern, Param, SpannedInst, Var};

    fn var(id: u32) -> VarId {
        VarId(id)
    }

    fn block(id: u32) -> BlockId {
        BlockId(id)
    }

    fn ident(s: &str) -> ast::Identifier {
        ast::Identifier(s.to_string())
    }

    /// Helper to wrap an instruction with a dummy span
    fn si(inst: Instruction) -> SpannedInst {
        ast::Spanned::new(inst, ast::Span::default())
    }

    fn make_function(blocks: Vec<BasicBlock>) -> Function {
        Function {
            blocks,
            ..Default::default()
        }
    }

    fn make_function_with_param(param_var: VarId, blocks: Vec<BasicBlock>) -> Function {
        Function {
            params: vec![Param {
                var: param_var,
                by_ref: false,
            }],
            blocks,
            ..Default::default()
        }
    }

    // ========================================================================
    // Basic Tests
    // ========================================================================

    #[test]
    fn test_const_has_single_type() {
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
        let analysis = analyze_types(&func, None);

        let type_set = analysis.get(var(0)).unwrap();
        assert!(type_set.contains(BaseType::UInt));
        assert!(type_set.is_single());
    }

    #[test]
    fn test_undefined_is_undefined_type() {
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Const {
                dest: var(0),
                value: Literal::Undefined,
            })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_types(&func, None);

        let type_set = analysis.get(var(0)).unwrap();
        assert_eq!(*type_set, TypeSet::undefined()); // Exactly {Undefined}
        assert!(type_set.may_be_undefined());
        assert!(!type_set.is_defined());
    }

    #[test]
    fn test_copy_inherits_type() {
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Bool(true),
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
        let analysis = analyze_types(&func, None);

        let type_set = analysis.get(var(1)).unwrap();
        assert!(type_set.contains(BaseType::Bool));
        assert!(type_set.is_single());
    }

    // ========================================================================
    // Control Flow Tests
    // ========================================================================

    #[test]
    fn test_match_with_narrowing_copies() {
        // SSA narrowing: Match on param var(0), each arm has a narrowing Copy
        // var(1) = Copy(var(0)) with UInt type in block 1
        // var(2) = Copy(var(0)) with Int type in block 2
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::any()),
            Var::new(var(1), ast::Identifier("$narrow".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("$narrow".into()), TypeSet::int()),
        ];
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::Match {
                    value: var(0),
                    arms: vec![
                        (MatchPattern::Type(BaseType::UInt), block(1)),
                        (MatchPattern::Type(BaseType::Int), block(2)),
                    ],
                    default: block(3),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![si(Instruction::Copy {
                    dest: var(1),
                    src: var(0),
                })],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Copy {
                    dest: var(2),
                    src: var(0),
                })],
                terminator: Terminator::Return {
                    value: Some(var(2)),
                },
            },
            BasicBlock {
                id: block(3),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
        ];

        let func = Function {
            name: ast::Identifier("test".into()),
            params: vec![Param {
                var: var(0),
                by_ref: false,
            }],
            rest_param: None,
            blocks,
            locals,
            entry_block: BlockId(0),
        };
        // mem2reg is not needed — IR is already in SSA form
        let analysis = analyze_types(&func, None);

        // var(1) should be UInt (narrowing copy in block 1)
        let type_1 = analysis.get(var(1)).unwrap();
        assert!(type_1.contains(BaseType::UInt));

        // var(2) should be Int (narrowing copy in block 2)
        let type_2 = analysis.get(var(2)).unwrap();
        assert!(type_2.contains(BaseType::Int));

        // var(0) is the param — type is any()
        let type_0 = analysis.get(var(0)).unwrap();
        assert!(type_0.may_be_undefined()); // any() includes Undefined
    }

    #[test]
    fn test_guard_with_narrowing_copy() {
        // SSA narrowing: Match on var(1) from Index, defined path has a
        // narrowing Copy var(2) with Undefined excluded.
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::any()),
            Var::new(var(1), ast::Identifier("idx_result".into()), TypeSet::any()),
            Var::new(
                var(2),
                ast::Identifier("$narrow".into()),
                TypeSet::defined(),
            ),
        ];
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Index {
                    dest: var(1),
                    base: var(0),
                    key: var(0),
                })],
                terminator: Terminator::Match {
                    value: var(1),
                    arms: vec![(MatchPattern::Type(BaseType::Undefined), block(2))],
                    default: block(1),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                // Narrowing copy: var(2) = Copy(var(1)) with defined() type
                instructions: vec![si(Instruction::Copy {
                    dest: var(2),
                    src: var(1),
                })],
                terminator: Terminator::Return {
                    value: Some(var(2)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = Function {
            name: ast::Identifier("test".into()),
            params: vec![Param {
                var: var(0),
                by_ref: false,
            }],
            rest_param: None,
            blocks,
            locals,
            entry_block: BlockId(0),
        };
        let analysis = analyze_types(&func, None);

        // var(2) is the narrowing copy — should be defined() (no Undefined)
        let type_2 = analysis.get(var(2)).unwrap();
        assert!(!type_2.is_empty());
        assert!(!type_2.contains(BaseType::Undefined));

        // var(1) is the Index result — could be anything including Undefined
        let type_1 = analysis.get(var(1)).unwrap();
        assert!(type_1.may_be_undefined());
    }

    #[test]
    fn test_phi_unions_types() {
        // if cond { x = 1u } else { x = "hello" }
        // After phi, x could be UInt or Text
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![],
                terminator: Terminator::If {
                    condition: var(0),
                    then_target: block(1),
                    else_target: block(2),
                    span: ast::Span::default(),
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
                    value: Literal::Text("hello".to_string()),
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
        let analysis = analyze_types(&func, None);

        // Phi result should be UInt | Text
        let type_set = analysis.get(var(3)).unwrap();
        assert!(type_set.contains(BaseType::UInt));
        assert!(type_set.contains(BaseType::Text));
        assert_eq!(type_set.len(), 2);
    }

    #[test]
    fn test_rest_param_is_array() {
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let func = Function {
            name: ident("test"),
            params: vec![Param {
                var: var(0),
                by_ref: false,
            }],
            rest_param: Some(Param {
                var: var(1),
                by_ref: false,
            }),
            locals: vec![],
            blocks,
            entry_block: block(0),
        };

        let analysis = analyze_types(&func, None);

        // Rest param should be Array type
        let type_set = analysis.get(var(1)).unwrap();
        assert!(type_set.contains(BaseType::Array));
        assert!(type_set.is_single());
    }
}
