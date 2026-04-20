use super::*;

pub(super) fn compile_terminator(
    term: &Terminator,
    block_map: &HashMap<BlockId, usize>,
    types: &TypeAnalysis,
    block_id: BlockId,
) -> Result<Step, ExecError> {
    Ok(match term {
        Terminator::Jump { target } => {
            let idx = block_map[target];
            Box::new(move |_vm, _prog| Ok(Action::NextBlock(idx)))
        }

        Terminator::If {
            condition,
            then_target,
            else_target,
            ..
        } => {
            let cond_slot = slot(*condition);
            let then_idx = block_map[then_target];
            let else_idx = block_map[else_target];

            let cond_type = types
                .get_at_exit(block_id, *condition)
                .copied()
                .unwrap_or(crate::types::TypeSet::any());

            // Non-Bool conditions should have been folded to Jump(else) by the
            // optimizer's fold_non_bool_conditions pass. is_dead() = unreachable
            // code where the type is bottom — not worth asserting on.
            debug_assert!(
                cond_type.contains(BaseType::Bool)
                    || cond_type.may_be_undefined()
                    || cond_type.is_dead(),
                "If condition with non-Bool type {:?} should have been folded by optimizer",
                cond_type
            );

            // All conditions go through the same path — Undefined is treated as false
            Box::new(move |vm: &mut VM, _prog| {
                let is_true = matches!(vm.local(cond_slot), Value::Bool(true));
                Ok(Action::NextBlock(if is_true { then_idx } else { else_idx }))
            })
        }

        Terminator::Match {
            value,
            arms,
            default,
            ..
        } => {
            let val_slot = slot(*value);
            let default_idx = block_map[default];
            compile_match(val_slot, arms, default_idx, block_map)
        }

        Terminator::Return { value } => {
            let val_slot = value.map(slot);
            Box::new(move |vm: &mut VM, _prog| {
                let val = val_slot
                    .map(|s| vm.local(s).clone())
                    .unwrap_or(Value::Undefined);
                Ok(Action::Return(val))
            })
        }

        Terminator::Exit { value } => {
            let val_slot = slot(*value);
            Box::new(move |vm: &mut VM, _prog| Ok(Action::Exit(vm.local(val_slot).clone())))
        }

        Terminator::Unreachable => Box::new(|_vm, _prog| Ok(Action::Return(Value::Undefined))),
    })
}

// ============================================================================
// Match Compilation
// ============================================================================

/// Compile a Match terminator, specializing based on arm count and pattern type.
///
/// - Single-arm type match: direct `base_type()` comparison (most common case from if-let)
/// - Single-arm literal: direct value comparison
/// - Single-arm array/array-min: direct length check
/// - Multi-arm: pre-compiled predicate closures (no MatchPattern dispatch at runtime)
pub(super) fn compile_match(
    val_slot: usize,
    arms: &[(MatchPattern, BlockId)],
    default_idx: usize,
    block_map: &HashMap<BlockId, usize>,
) -> Step {
    if arms.len() == 1 {
        // Single-arm fast path — inline the pattern test directly
        let target_idx = block_map[&arms[0].1];
        return compile_single_arm_match(val_slot, &arms[0].0, target_idx, default_idx);
    }

    // Multi-arm: pre-compile each pattern into a predicate closure
    #[allow(clippy::type_complexity)]
    let compiled_arms: Vec<(Box<dyn Fn(&Value) -> bool>, usize)> = arms
        .iter()
        .map(|(pat, target)| (compile_match_predicate(pat), block_map[target]))
        .collect();

    Box::new(move |vm: &mut VM, _prog| {
        let val = vm.local(val_slot);
        for (predicate, target_idx) in &compiled_arms {
            if predicate(val) {
                return Ok(Action::NextBlock(*target_idx));
            }
        }
        Ok(Action::NextBlock(default_idx))
    })
}

/// Compile a single-arm Match into a direct test — no Vec, no predicate dispatch.
pub(super) fn compile_single_arm_match(
    val_slot: usize,
    pattern: &MatchPattern,
    target_idx: usize,
    default_idx: usize,
) -> Step {
    match pattern {
        MatchPattern::Type(base_type) => {
            let ty = *base_type;
            Box::new(move |vm: &mut VM, _prog| {
                let matched = vm.local(val_slot).base_type() == ty;
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
        MatchPattern::Literal(lit) => {
            let pred = compile_match_predicate(&MatchPattern::Literal(lit.clone()));
            Box::new(move |vm: &mut VM, _prog| {
                let matched = pred(vm.local(val_slot));
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
        MatchPattern::Array(len) => {
            let expected = *len;
            Box::new(move |vm: &mut VM, _prog| {
                let matched = matches!(vm.local(val_slot), Value::Array(a) if a.len() == expected);
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
        MatchPattern::ArrayMin(min) => {
            let expected = *min;
            Box::new(move |vm: &mut VM, _prog| {
                let matched = matches!(vm.local(val_slot), Value::Array(a) if a.len() >= expected);
                Ok(Action::NextBlock(if matched {
                    target_idx
                } else {
                    default_idx
                }))
            })
        }
    }
}

/// Pre-compile a MatchPattern into a predicate closure for multi-arm dispatch.
/// The MatchPattern enum is resolved at compile time — the returned closure
/// does only the value-level test with no pattern variant dispatch.
pub(super) fn compile_match_predicate(pattern: &MatchPattern) -> Box<dyn Fn(&Value) -> bool> {
    match pattern {
        MatchPattern::Type(base_type) => {
            let ty = *base_type;
            Box::new(move |v| v.base_type() == ty)
        }
        MatchPattern::Literal(lit) => match lit {
            Literal::Bool(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::Bool(b) if *b == e))
            }
            Literal::UInt(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::UInt(n) if *n == e))
            }
            Literal::Int(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::Int(n) if *n == e))
            }
            Literal::Float(expected) => {
                let e = *expected;
                Box::new(move |v| matches!(v, Value::Float(f) if f.get() == e))
            }
            Literal::Text(expected) => {
                let e = expected.clone();
                Box::new(move |v| matches!(v, Value::Text(s) if **s == *e))
            }
            Literal::Bytes(expected) => {
                let e = expected.clone();
                Box::new(move |v| matches!(v, Value::Bytes(b) if **b == *e))
            }
            Literal::Undefined => Box::new(|v| v.is_undefined()),
        },
        MatchPattern::Array(len) => {
            let expected = *len;
            Box::new(move |v| matches!(v, Value::Array(a) if a.len() == expected))
        }
        MatchPattern::ArrayMin(min) => {
            let expected = *min;
            Box::new(move |v| matches!(v, Value::Array(a) if a.len() >= expected))
        }
    }
}
