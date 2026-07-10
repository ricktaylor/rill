//! Constant Folding Optimization
//!
//! Evaluates expressions at compile time when all operands are known constants.
//! This pass:
//! - Tracks which variables hold constant values
//! - Replaces extern calls with `Purity::Const` with their evaluated result
//! - Folds intrinsic operations (And, Or) on constant operands
//! - Propagates constants through Copy instructions
//! - Simplifies control flow when conditions are constant
//!
//! This pass should be run multiple times during optimization:
//! - After lowering (fold obvious constants)
//! - After DCE (may expose new constants)
//! - After inlining (arguments become constants in inlined code)

use crate::diagnostics::{DiagnosticCode, Diagnostics};
use crate::externs::ExternRegistry;
use crate::ir::const_eval::{
    const_index, const_to_literal, eval_extern_const, eval_intrinsic_const, literal_to_const,
};
use crate::ir::{BlockId, Function, FunctionRef, Instruction, Terminator, VarId};
use crate::ir::{ConstValue, IntrinsicOp};
use std::collections::HashMap;

/// Map from VarId to its constant value (if known)
pub type ConstantMap = HashMap<VarId, ConstValue>;

/// Fold constants in a function
///
/// Replaces instructions with constant results and simplifies control flow
/// when conditions are constant. Emits warnings for redundant guards and
/// unreachable match arms.
/// Returns the number of instructions/terminators folded.
pub fn fold_constants(
    function: &mut Function,
    externs: &ExternRegistry,
    diagnostics: &mut Diagnostics,
) -> usize {
    let mut constants: ConstantMap = HashMap::new();
    let mut folded = 0;

    // Collect constant values, iterating to fixpoint.
    // With proper SSA, each VarId is assigned exactly once, so this
    // converges in 1-2 iterations.
    loop {
        let prev_count = constants.len();
        collect_constants(function, &mut constants, externs);
        if constants.len() == prev_count {
            break;
        }
    }

    // Second pass: replace instructions that produce constants
    for block in &mut function.blocks {
        for spanned_inst in &mut block.instructions {
            if let Some(replacement) = try_fold_instruction(&spanned_inst.node, &constants, externs)
            {
                spanned_inst.node = replacement;
                folded += 1;
            }
        }

        // Third pass: simplify terminators with constant conditions
        if let Some(simplified) =
            try_simplify_terminator(&block.terminator, &constants, &function.name, diagnostics)
        {
            block.terminator = simplified;
            folded += 1;
        }
    }

    folded
}

/// Collect constant values from instructions (SSA: each VarId assigned once)
fn collect_constants(function: &Function, constants: &mut ConstantMap, externs: &ExternRegistry) {
    // Build Copy chain map for resolving through Copies.
    let copy_sources: HashMap<VarId, VarId> = function
        .blocks
        .iter()
        .flat_map(|b| b.instructions.iter())
        .filter_map(|inst| {
            if let Instruction::Copy { dest, src } = &inst.node {
                Some((*dest, *src))
            } else {
                None
            }
        })
        .collect();

    // Follow Copy chains to find the ultimate source VarId.
    let resolve_copies = |mut var: VarId| -> VarId {
        for _ in 0..64 {
            match copy_sources.get(&var) {
                Some(&src) => var = src,
                None => break,
            }
        }
        var
    };

    // Build ref base map for precise WriteRef invalidation.
    // Maps MakeAccessor/MakeRef dest → ULTIMATE base VarId (following Copy chains).
    let ref_bases: HashMap<VarId, VarId> = function
        .blocks
        .iter()
        .flat_map(|b| b.instructions.iter())
        .filter_map(|inst| match &inst.node {
            Instruction::MakeAccessor { dest, base, .. }
            | Instruction::MakeRef { dest, base, .. } => Some((*dest, resolve_copies(*base))),
            _ => None,
        })
        .collect();

    for block in &function.blocks {
        for spanned_inst in &block.instructions {
            match &spanned_inst.node {
                Instruction::Const { dest, value } => {
                    if let Some(cv) = literal_to_const(value) {
                        constants.insert(*dest, cv);
                    }
                }

                Instruction::Copy { dest, src } => {
                    if let Some(cv) = constants.get(src) {
                        constants.insert(*dest, cv.clone());
                    }
                }

                Instruction::Phi { dest, sources } => {
                    if let Some(cv) = try_fold_phi(sources, constants) {
                        constants.insert(*dest, cv);
                    }
                }

                Instruction::Call {
                    dest,
                    function: func_ref,
                    args,
                } => {
                    if let Some(cv) = try_fold_call(func_ref, args, constants, externs) {
                        constants.insert(*dest, cv);
                    }
                }

                Instruction::Intrinsic { dest, op, args } => {
                    if let Some(cv) = try_fold_intrinsic(*op, args, constants) {
                        constants.insert(*dest, cv);
                    }
                }

                Instruction::Index { dest, base, key } => {
                    if let Some(cv) = try_fold_index(*base, *key, constants) {
                        constants.insert(*dest, cv);
                    }
                }

                // Mutating instructions invalidate the target's constant value.
                // After `append(arr, val)` or `arr[i] = val`, arr is no longer
                // the original constant — its contents have changed. The
                // written operand may reach the real collection through Copy
                // chains or a MakeRef wrapper (pattern bindings wrap the
                // scrutinee in a ref), so invalidate the resolved ultimate
                // base as well.
                Instruction::Append { arr, .. } => {
                    constants.remove(arr);
                    let resolved = resolve_copies(*arr);
                    constants.remove(&resolved);
                    if let Some(base) = ref_bases.get(arr).or_else(|| ref_bases.get(&resolved)) {
                        constants.remove(base);
                    }
                }
                Instruction::WriteAccessor { base, .. } => {
                    constants.remove(base);
                    let resolved = resolve_copies(*base);
                    constants.remove(&resolved);
                    if let Some(ultimate) = ref_bases.get(base).or_else(|| ref_bases.get(&resolved))
                    {
                        constants.remove(ultimate);
                    }
                }
                Instruction::WriteRef { ref_var, .. } => {
                    // WriteRef mutates through a MakeRef. Trace the ref_var
                    // back to the MakeRef's base and invalidate that.
                    if let Some(base) = ref_bases.get(ref_var) {
                        constants.remove(base);
                    } else {
                        // Ref created externally (by-ref param) — whole-value
                        // ref that writes to the caller's frame. Only the
                        // ref target itself needs invalidation, not local
                        // collections.
                        constants.remove(ref_var);
                    }
                }

                _ => {}
            }
        }
    }
}

/// Try to fold an instruction into a Const instruction
fn try_fold_instruction(
    instruction: &Instruction,
    constants: &ConstantMap,
    externs: &ExternRegistry,
) -> Option<Instruction> {
    match instruction {
        // Already a Const - nothing to fold
        Instruction::Const { .. } => None,

        // Copy from a constant -> replace with Const
        Instruction::Copy { dest, src } => {
            let cv = constants.get(src)?;
            let lit = const_to_literal(cv)?;
            Some(Instruction::Const {
                dest: *dest,
                value: lit,
            })
        }

        // Call with constant result -> replace with Const
        Instruction::Call {
            dest,
            function: func_ref,
            args,
        } => {
            let cv = try_fold_call(func_ref, args, constants, externs)?;
            let lit = const_to_literal(&cv)?;
            Some(Instruction::Const {
                dest: *dest,
                value: lit,
            })
        }

        // Intrinsic with constant result -> replace with Const
        Instruction::Intrinsic { dest, op, args } => {
            let cv = try_fold_intrinsic(*op, args, constants)?;
            let lit = const_to_literal(&cv)?;
            Some(Instruction::Const {
                dest: *dest,
                value: lit,
            })
        }

        // Phi with constant result -> replace with Const
        Instruction::Phi { dest, sources } => {
            let cv = try_fold_phi(sources, constants)?;
            let lit = const_to_literal(&cv)?;
            Some(Instruction::Const {
                dest: *dest,
                value: lit,
            })
        }

        // Index with constant result -> replace with Const
        Instruction::Index { dest, base, key } => {
            let cv = try_fold_index(*base, *key, constants)?;
            let lit = const_to_literal(&cv)?;
            Some(Instruction::Const {
                dest: *dest,
                value: lit,
            })
        }

        // Other instructions can't be folded
        _ => None,
    }
}

/// Try to simplify a terminator with constant conditions
/// Emits warnings for redundant guards and unreachable match arms
fn try_simplify_terminator(
    terminator: &Terminator,
    constants: &ConstantMap,
    func_name: &crate::ast::Identifier,
    diagnostics: &mut Diagnostics,
) -> Option<Terminator> {
    match terminator {
        Terminator::If {
            condition,
            then_target,
            else_target,
            ..
        } => {
            if let Some(ConstValue::Bool(b)) = constants.get(condition) {
                Some(Terminator::Jump {
                    target: if *b { *then_target } else { *else_target },
                })
            } else {
                None
            }
        }

        // Match with constant scrutinee
        Terminator::Match {
            value,
            arms,
            default,
            span,
        } => {
            if let Some(cv) = constants.get(value) {
                // Find the matching arm
                let mut matching_idx = None;
                for (idx, (pattern, _)) in arms.iter().enumerate() {
                    if pattern_matches(pattern, cv) {
                        matching_idx = Some(idx);
                        break;
                    }
                }

                let (target, unreachable_count) = match matching_idx {
                    Some(idx) => {
                        // Arms after the match are unreachable, plus default
                        (arms[idx].1, arms.len() - idx - 1 + 1)
                    }
                    None => {
                        // All arms are unreachable, go to default
                        (*default, arms.len())
                    }
                };

                if unreachable_count > 0 && *span != crate::ast::Span::default() {
                    diagnostics.warning(
                        DiagnosticCode::W003_UnreachableCode,
                        *span,
                        format!(
                            "in function `{}`: match has {} unreachable arm(s)",
                            func_name, unreachable_count
                        ),
                    );
                }

                Some(Terminator::Jump { target })
            } else {
                None
            }
        }

        _ => None,
    }
}

/// Check if a match pattern matches a constant value
fn pattern_matches(pattern: &crate::ir::MatchPattern, value: &ConstValue) -> bool {
    use crate::ir::MatchPattern;

    match (pattern, value) {
        (MatchPattern::Literal(lit), cv) => literal_to_const(lit).as_ref() == Some(cv),
        (MatchPattern::Type(base_type), cv) => {
            use crate::types::BaseType;
            match (base_type, cv) {
                (BaseType::Bool, ConstValue::Bool(_)) => true,
                (BaseType::UInt, ConstValue::UInt(_)) => true,
                (BaseType::Int, ConstValue::Int(_)) => true,
                (BaseType::Float, ConstValue::Float(_)) => true,
                (BaseType::Text, ConstValue::Text(_)) => true,
                (BaseType::Bytes, ConstValue::Bytes(_)) => true,
                (BaseType::Array, ConstValue::Array(_)) => true,
                (BaseType::Map, ConstValue::Map(_)) => true,
                // Sequence is a lazy runtime type — no ConstValue representation,
                // so it can never match a constant value
                (BaseType::Sequence, _) => false,
                _ => false,
            }
        }
        (MatchPattern::Array(len), ConstValue::Array(arr)) => arr.len() == *len,
        (MatchPattern::ArrayMin(min_len), ConstValue::Array(arr)) => arr.len() >= *min_len,
        _ => false,
    }
}

/// Try to fold an extern call with constant arguments
fn try_fold_call(
    func_ref: &FunctionRef,
    args: &[VarId],
    constants: &ConstantMap,
    externs: &ExternRegistry,
) -> Option<ConstValue> {
    // Collect constant arguments
    let const_args: Option<Vec<ConstValue>> =
        args.iter().map(|arg| constants.get(arg).cloned()).collect();

    let const_args = const_args?;

    // Use shared helper
    eval_extern_const(func_ref, &const_args, externs)
}

/// Try to fold an intrinsic operation with constant arguments
fn try_fold_intrinsic(
    op: IntrinsicOp,
    args: &[VarId],
    constants: &ConstantMap,
) -> Option<ConstValue> {
    // All arguments must be constant
    let const_args: Option<Vec<ConstValue>> =
        args.iter().map(|v| constants.get(v).cloned()).collect();
    eval_intrinsic_const(op, &const_args?)
}

/// Try to fold a Phi with all constant sources of the same value
fn try_fold_phi(sources: &[(BlockId, VarId)], constants: &ConstantMap) -> Option<ConstValue> {
    if sources.is_empty() {
        return None;
    }

    // Get the first constant value
    let first = constants.get(&sources[0].1)?;

    // Check if all sources have the same constant value
    for (_, var) in &sources[1..] {
        match constants.get(var) {
            Some(cv) if cv == first => continue,
            _ => return None,
        }
    }

    Some(first.clone())
}

/// Try to fold an index operation with constant base and key
fn try_fold_index(base: VarId, key: VarId, constants: &ConstantMap) -> Option<ConstValue> {
    let base_cv = constants.get(&base)?;
    let key_cv = constants.get(&key)?;
    const_index(base_cv, key_cv)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::diagnostics::Diagnostics;
    use crate::externs::standard_externs;
    use crate::ir::{BasicBlock, Literal, SpannedInst};

    fn var(id: u32) -> VarId {
        VarId(id)
    }

    fn block(id: u32) -> BlockId {
        BlockId(id)
    }

    /// Helper to wrap an instruction with a default span
    fn si(inst: Instruction) -> SpannedInst {
        ast::Spanned::new(inst, ast::Span::default())
    }

    fn make_function(blocks: Vec<BasicBlock>) -> Function {
        Function {
            blocks,
            ..Default::default()
        }
    }

    #[test]
    fn test_fold_copy_of_constant() {
        let externs = standard_externs();
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

        let mut func = make_function(blocks);
        let mut diags = Diagnostics::new();
        fold_constants(&mut func, &externs, &mut diags);

        // var(1) should now be a Const instruction
        assert!(matches!(
            &func.blocks[0].instructions[1].node,
            Instruction::Const { dest, value: Literal::UInt(42) } if *dest == var(1)
        ));
    }

    #[test]
    fn test_fold_if_constant_true() {
        let externs = standard_externs();
        let blocks = vec![
            BasicBlock {
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

        let mut func = make_function(blocks);
        let mut diags = Diagnostics::new();
        fold_constants(&mut func, &externs, &mut diags);

        // Block 0's terminator should be a jump to block 1
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(1)
        ));
    }

    #[test]
    fn test_fold_if_constant_false() {
        let externs = standard_externs();
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::Bool(false),
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

        let mut func = make_function(blocks);
        let mut diags = Diagnostics::new();
        fold_constants(&mut func, &externs, &mut diags);

        // Block 0's terminator should be a jump to block 2
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(2)
        ));
    }

    #[test]
    fn test_fold_guard_on_constant() {
        use crate::ir::MatchPattern;
        use crate::types::BaseType;

        let externs = standard_externs();
        let blocks = vec![
            BasicBlock {
                id: block(0),
                instructions: vec![si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::Match {
                    value: var(0),
                    arms: vec![(MatchPattern::Type(BaseType::Undefined), block(2))],
                    default: block(1),
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

        let mut func = make_function(blocks);
        let mut diags = Diagnostics::new();
        fold_constants(&mut func, &externs, &mut diags);

        // Constants are always defined (UInt, not Undefined), so jump to default (block 1)
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Jump { target } if target == block(1)
        ));
    }

    #[test]
    fn test_fold_phi_same_constants() {
        let externs = standard_externs();
        // Simulate: if cond { 42 } else { 42 } -> always 42
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
                    value: Literal::UInt(42),
                })],
                terminator: Terminator::Jump { target: block(3) },
            },
            BasicBlock {
                id: block(2),
                instructions: vec![si(Instruction::Const {
                    dest: var(2),
                    value: Literal::UInt(42),
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

        let mut func = make_function(blocks);
        let mut diags = Diagnostics::new();
        fold_constants(&mut func, &externs, &mut diags);

        // var(3) should now be a Const 42
        assert!(matches!(
            &func.blocks[3].instructions[0].node,
            Instruction::Const { dest, value: Literal::UInt(42) } if *dest == var(3)
        ));
    }

    #[test]
    fn test_fold_array_index() {
        let mut constants: ConstantMap = HashMap::new();
        constants.insert(
            var(0),
            ConstValue::Array(vec![
                ConstValue::UInt(10),
                ConstValue::UInt(20),
                ConstValue::UInt(30),
            ]),
        );
        constants.insert(var(1), ConstValue::UInt(1));

        let result = try_fold_index(var(0), var(1), &constants);
        assert_eq!(result, Some(ConstValue::UInt(20)));
    }
}
