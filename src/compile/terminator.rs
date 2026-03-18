use super::*;

pub(super) fn compile_terminator(
    term: &Terminator,
    block_map: &HashMap<BlockId, usize>,
    types: &TypeAnalysis,
    defs: &crate::ir::opt::DefinednessAnalysis,
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
                .unwrap_or(crate::types::TypeSet::all());
            let cond_def = defs.get_at_exit(block_id, *condition);

            // Non-Bool conditions should have been folded to Jump(else) by the
            // optimizer's fold_non_bool_conditions pass.
            debug_assert!(
                cond_type.contains(BaseType::Bool) || cond_type.is_empty(),
                "If condition with non-Bool type {:?} should have been folded by optimizer",
                cond_type
            );

            // Provably Bool and Defined → skip null + type checks
            if cond_type.is_single()
                && cond_type.contains(BaseType::Bool)
                && cond_def == crate::ir::opt::Definedness::Defined
            {
                return Ok(Box::new(move |vm: &mut VM, _prog| {
                    let is_true = match vm.local(cond_slot).unwrap() {
                        Value::Bool(b) => *b,
                        _ => unreachable!(),
                    };
                    Ok(Action::NextBlock(if is_true { then_idx } else { else_idx }))
                }));
            }

            Box::new(move |vm: &mut VM, _prog| {
                let is_true = vm
                    .local(cond_slot)
                    .map(|v| matches!(v, Value::Bool(true)))
                    .unwrap_or(false);
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

        Terminator::Guard {
            value,
            defined,
            undefined,
            ..
        } => {
            // Guards with known definedness should have been folded to Jump
            // by the optimizer's eliminate_guards pass.
            debug_assert!(
                defs.get_at_exit(block_id, *value) == crate::ir::opt::Definedness::MaybeDefined,
                "Guard on {:?} with definedness {:?} should have been eliminated by optimizer",
                value,
                defs.get_at_exit(block_id, *value)
            );

            let val_slot = slot(*value);
            let def_idx = block_map[defined];
            let undef_idx = block_map[undefined];
            Box::new(move |vm: &mut VM, _prog| {
                let is_defined = vm.local(val_slot).is_some();
                Ok(Action::NextBlock(if is_defined {
                    def_idx
                } else {
                    undef_idx
                }))
            })
        }

        Terminator::Return { value } => {
            let val_slot = value.map(slot);
            Box::new(move |vm: &mut VM, _prog| {
                let val = val_slot.and_then(|s| vm.local(s).cloned());
                Ok(Action::Return(val))
            })
        }

        Terminator::Exit { value } => {
            let val_slot = slot(*value);
            Box::new(move |vm: &mut VM, _prog| {
                let val = vm.local(val_slot).cloned().unwrap_or(Value::UInt(0));
                Ok(Action::Exit(val))
            })
        }

        Terminator::Unreachable => Box::new(|_vm, _prog| Ok(Action::Return(None))),
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
        if let Some(val) = vm.local(val_slot) {
            for (predicate, target_idx) in &compiled_arms {
                if predicate(val) {
                    return Ok(Action::NextBlock(*target_idx));
                }
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
                let matched = vm.local(val_slot).is_some_and(|v| v.base_type() == ty);
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
                let matched = vm.local(val_slot).is_some_and(&pred);
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
                let matched = vm
                    .local(val_slot)
                    .is_some_and(|v| matches!(v, Value::Array(a) if a.len() == expected));
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
                let matched = vm
                    .local(val_slot)
                    .is_some_and(|v| matches!(v, Value::Array(a) if a.len() >= expected));
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
