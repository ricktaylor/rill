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
//! - Call instructions: use builtin metadata for return types

use super::{BlockId, CallArg, Function, FunctionRef, Instruction, Terminator, VarId};
use crate::builtins::BuiltinRegistry;
use crate::ir::types::{BaseType, TypeSet};
use std::collections::{HashMap, HashSet, VecDeque};

// ============================================================================
// Analysis State
// ============================================================================

/// Map from (BlockId, VarId) to TypeSet at block entry
pub type TypeMap = HashMap<(BlockId, VarId), TypeSet>;

/// Analysis result for a function
#[derive(Debug)]
pub struct TypeAnalysis {
    /// TypeSet of each variable at each block's entry point
    #[allow(dead_code)]
    pub at_entry: TypeMap,

    /// TypeSet of each variable at each block's exit point
    pub at_exit: TypeMap,
}

impl TypeAnalysis {
    /// Get the TypeSet of a variable at a block's entry
    #[allow(dead_code)]
    pub fn get_at_entry(&self, block: BlockId, var: VarId) -> Option<&TypeSet> {
        self.at_entry.get(&(block, var))
    }

    /// Get the TypeSet of a variable at a block's exit
    pub fn get_at_exit(&self, block: BlockId, var: VarId) -> Option<&TypeSet> {
        self.at_exit.get(&(block, var))
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
    builtins: Option<&BuiltinRegistry>,
    return_types: &ReturnTypes,
) {
    match instruction {
        // Constants have known single types
        Instruction::Const { dest, value } => {
            let ty = match value {
                crate::ir::Literal::Bool(_) => BaseType::Bool,
                crate::ir::Literal::UInt(_) => BaseType::UInt,
                crate::ir::Literal::Int(_) => BaseType::Int,
                crate::ir::Literal::Float(_) => BaseType::Float,
                crate::ir::Literal::Text(_) => BaseType::Text,
                crate::ir::Literal::Bytes(_) => BaseType::Bytes,
            };
            state.insert(*dest, TypeSet::single(ty));
        }

        // Undefined produces only undefined (no concrete types)
        Instruction::Undefined { dest } => {
            state.insert(*dest, TypeSet::empty());
        }

        // Copy inherits the type of the source
        Instruction::Copy { dest, src } => {
            if let Some(src_type) = state.get(src) {
                state.insert(*dest, *src_type);
            } else {
                // Unknown source - use all types as optional
                state.insert(*dest, all_types());
            }
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

        // Intrinsic: refine result type based on operand types
        Instruction::Intrinsic { dest, op, args } => {
            let arg_types: Vec<TypeSet> = args
                .iter()
                .map(|v| state.get(v).cloned().unwrap_or_else(all_types))
                .collect();
            state.insert(*dest, op.result_type_refined(&arg_types));
        }

        // Call: use builtin metadata if available
        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let type_set = compute_call_type(function, args, state, builtins, return_types);
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

        // Drop doesn't produce a value
        Instruction::Drop { .. } => {}
    }
}

/// Compute the return type of a function call using builtin metadata
fn compute_call_type(
    function: &FunctionRef,
    _args: &[CallArg],
    _state: &HashMap<VarId, TypeSet>,
    builtins: Option<&BuiltinRegistry>,
    return_types: &ReturnTypes,
) -> TypeSet {
    let Some(builtin) = builtins.and_then(|r| r.lookup(function)) else {
        // Not a builtin — check inferred return types for user functions
        let name = function.qualified_name();
        if let Some(rt) = return_types.get(&name)
            && !rt.is_empty()
        {
            return *rt;
        }
        // Not a builtin and no inferred return type yet.
        // During Phase A (per-function): return types haven't been collected.
        // During Phase B (interprocedural): recursive calls or functions
        // analyzed later in the iteration — conservatively return all types.
        // Truly undefined functions are caught by the link phase (E500).
        return all_types();
    };

    // If the function diverges, it never returns (empty type set)
    if builtin.meta.diverges() {
        return TypeSet::empty();
    }

    // Get the return type signature and convert to TypeSet
    // Note: fallibility (may_return_undefined) is tracked by Definedness analysis
    type_sig_to_type_set(builtin.meta.returns.type_sig())
}

/// Convert a builtin's TypeSet to analysis TypeSet
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
    let mut refined = HashMap::new();

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
                    };
                    TypeSet::single(ty)
                }
                crate::ir::MatchPattern::Array(_) | crate::ir::MatchPattern::ArrayMin(_) => {
                    TypeSet::single(BaseType::Array)
                }
            };
            arm_state.insert(*value, refined_type);
            refined.insert(*target, arm_state);
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

/// Apply type refinement at a Guard terminator
///
/// Guard doesn't refine types - it only affects definedness which is
/// tracked by the separate Definedness analysis. The TypeSet remains
/// unchanged in both branches (the type of a value doesn't change based
/// on whether it's defined or not).
fn apply_guard_refinement(
    terminator: &Terminator,
    state: &HashMap<VarId, TypeSet>,
) -> HashMap<BlockId, HashMap<VarId, TypeSet>> {
    let mut refined = HashMap::new();

    if let Terminator::Guard {
        defined, undefined, ..
    } = terminator
    {
        // Both branches keep the same type information
        // (definedness is tracked separately)
        refined.insert(*defined, state.clone());
        refined.insert(*undefined, state.clone());
    }

    refined
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Create a TypeSet containing all base types
fn all_types() -> TypeSet {
    TypeSet::all()
}

// ============================================================================
// Main Analysis
// ============================================================================

/// Inferred return types for user-defined functions.
pub type ReturnTypes = std::collections::HashMap<String, TypeSet>;

/// Infer the return type of a function from its Return terminators.
///
/// Runs type analysis, then unions the TypeSets of all Return values.
/// Functions with no return value (all returns are `None`) produce `TypeSet::empty()`.
pub fn infer_return_type(
    function: &Function,
    builtins: Option<&BuiltinRegistry>,
    return_types: &ReturnTypes,
) -> TypeSet {
    let types = analyze_types_with_returns(function, builtins, return_types);

    let mut result = TypeSet::empty();
    for block in &function.blocks {
        if let Terminator::Return { value: Some(v) } = &block.terminator
            && let Some(ts) = types.get_at_exit(block.id, *v)
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
pub fn analyze_types(function: &Function, builtins: Option<&BuiltinRegistry>) -> TypeAnalysis {
    analyze_types_with_returns(function, builtins, &ReturnTypes::new())
}

/// Analyze types with interprocedural return type information.
pub fn analyze_types_with_returns(
    function: &Function,
    builtins: Option<&BuiltinRegistry>,
    return_types: &ReturnTypes,
) -> TypeAnalysis {
    let block_index = build_block_index_map(function);

    // State at entry and exit of each block
    let mut entry_states: HashMap<BlockId, HashMap<VarId, TypeSet>> = HashMap::new();
    let mut exit_states: HashMap<BlockId, HashMap<VarId, TypeSet>> = HashMap::new();

    // Initialize entry block with parameter types
    let mut initial_state = HashMap::new();
    for param in &function.params {
        // Parameters can be any type (caller decides)
        initial_state.insert(param.var, all_types());
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
            transfer_instruction(&spanned_inst.node, &mut state, builtins, return_types);
        }

        // Check if exit state changed
        let old_exit = exit_states.get(&block_id);
        let changed = old_exit.is_none_or(|old| *old != state);

        if changed {
            exit_states.insert(block_id, state.clone());

            // Apply control flow refinement
            let match_refined = apply_match_refinement(&block.terminator, &state);
            let guard_refined = apply_guard_refinement(&block.terminator, &state);

            // Propagate to successors
            for succ_id in block.terminator.successors() {
                // Compute new entry state for successor
                let new_entry = match_refined
                    .get(&succ_id)
                    .or_else(|| guard_refined.get(&succ_id))
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

    // Convert to the public format
    let mut at_entry = TypeMap::new();
    let mut at_exit = TypeMap::new();

    for (block_id, state) in entry_states {
        for (var, type_set) in state {
            at_entry.insert((block_id, var), type_set);
        }
    }

    for (block_id, state) in exit_states {
        for (var, type_set) in state {
            at_exit.insert((block_id, var), type_set);
        }
    }

    TypeAnalysis { at_entry, at_exit }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, Literal, MatchPattern, Param, SpannedInst};

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

        let type_set = analysis.get_at_exit(block(0), var(0)).unwrap();
        assert!(type_set.contains(BaseType::UInt));
        assert!(type_set.is_single());
    }

    #[test]
    fn test_undefined_has_no_concrete_type() {
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Undefined { dest: var(0) })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let func = make_function(blocks);
        let analysis = analyze_types(&func, None);

        let type_set = analysis.get_at_exit(block(0), var(0)).unwrap();
        assert!(type_set.is_empty()); // No concrete types for undefined
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

        let type_set = analysis.get_at_exit(block(0), var(1)).unwrap();
        assert!(type_set.contains(BaseType::Bool));
        assert!(type_set.is_single());
    }

    // ========================================================================
    // Control Flow Tests
    // ========================================================================

    #[test]
    fn test_match_refines_type() {
        // match x {
        //   uint: block1,
        //   int: block2,
        //   _: block3
        // }
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
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(0)),
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

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_types(&func, None);

        // In block 1, var(0) should be UInt only
        let type_1 = analysis.get_at_entry(block(1), var(0)).unwrap();
        assert!(type_1.contains(BaseType::UInt));
        assert!(type_1.is_single());

        // In block 2, var(0) should be Int only
        let type_2 = analysis.get_at_entry(block(2), var(0)).unwrap();
        assert!(type_2.contains(BaseType::Int));
        assert!(type_2.is_single());

        // In block 3 (default), var(0) should NOT include UInt or Int
        let type_3 = analysis.get_at_entry(block(3), var(0)).unwrap();
        assert!(!type_3.contains(BaseType::UInt));
        assert!(!type_3.contains(BaseType::Int));
    }

    #[test]
    fn test_guard_does_not_refine_types() {
        // Guard only affects definedness (tracked separately), not types
        // guard x -> defined: block1, undefined: block2
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Index {
                    dest: var(1),
                    base: var(0),
                    key: var(0),
                })],
                terminator: Terminator::Guard {
                    value: var(1),
                    defined: block(1),
                    undefined: block(2),
                    span: ast::Span::default(),
                },
            },
            BasicBlock {
                id: block(1),
                instructions: vec![],
                terminator: Terminator::Return {
                    value: Some(var(1)),
                },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![],
                terminator: Terminator::Return { value: None },
            },
        ];

        let func = make_function_with_param(var(0), blocks);
        let analysis = analyze_types(&func, None);

        // Both branches should have the same type for var(1)
        // (definedness is tracked by Definedness analysis, not here)
        let type_1 = analysis.get_at_entry(block(1), var(1)).unwrap();
        let type_2 = analysis.get_at_entry(block(2), var(1)).unwrap();

        // Index returns all types (we don't know element type)
        assert!(!type_1.is_empty());
        assert_eq!(type_1, type_2); // Same types in both branches
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
        let type_set = analysis.get_at_exit(block(3), var(3)).unwrap();
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
        let type_set = analysis.get_at_entry(block(0), var(1)).unwrap();
        assert!(type_set.contains(BaseType::Array));
        assert!(type_set.is_single());
    }
}
