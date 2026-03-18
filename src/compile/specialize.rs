use super::*;

// ============================================================================
// Intrinsic Runtime Execution
// ============================================================================

/// Try to emit a type-specialized closure for a binary arithmetic intrinsic.
///
/// Consults TypeAnalysis: if both operands are provably a single numeric type
/// and they match, emits a direct closure that skips the 10-way runtime
/// type dispatch. Returns `None` to fall back to `compile_intrinsic_dispatch`.
pub(super) fn try_specialize_binary(
    op: IntrinsicOp,
    arg_slots: &[usize],
    dest_slot: usize,
    args: &[VarId],
    types: &TypeAnalysis,
    block_id: BlockId,
) -> Option<Step> {
    // Only specialize binary arithmetic and comparison
    if args.len() != 2 {
        return None;
    }
    if !matches!(
        op,
        IntrinsicOp::Add
            | IntrinsicOp::Sub
            | IntrinsicOp::Mul
            | IntrinsicOp::Div
            | IntrinsicOp::Mod
            | IntrinsicOp::Lt
            | IntrinsicOp::Eq
    ) {
        return None;
    }

    let a_type = types.get_at_exit(block_id, args[0])?;
    let b_type = types.get_at_exit(block_id, args[1])?;

    // Both must be single and the same type
    if !a_type.is_single() || !b_type.is_single() || a_type != b_type {
        return None;
    }

    let a = arg_slots[0];
    let b = arg_slots[1];
    let d = dest_slot;

    // Determine the single type
    if a_type.contains(BaseType::UInt) {
        Some(specialize_uint(op, a, b, d))
    } else if a_type.contains(BaseType::Int) {
        Some(specialize_int(op, a, b, d))
    } else if a_type.contains(BaseType::Float) {
        Some(specialize_float(op, a, b, d))
    } else {
        None
    }
}

/// Try to emit a type-specialized closure for a Cast intrinsic.
///
/// Consults TypeAnalysis for the source type and the const map for the target
/// type code. Three levels of specialization:
///
/// 1. Source type + target both known → fully specialized (identity copy or
///    single direct conversion, zero dispatch at runtime)
/// 2. Target known, source unknown → target-specialized closure that only
///    dispatches on source value type (eliminates target slot read + target match)
/// 3. Neither known → falls through to `compile_intrinsic_dispatch`
pub(super) fn try_specialize_cast(
    op: IntrinsicOp,
    arg_slots: &[usize],
    dest_slot: usize,
    args: &[VarId],
    types: &TypeAnalysis,
    block_id: BlockId,
    consts: &HashMap<VarId, u64>,
) -> Option<Step> {
    if op != IntrinsicOp::Cast || args.len() != 2 {
        return None;
    }

    let target = *consts.get(&args[1])?;
    let src = arg_slots[0];
    let d = dest_slot;

    // Check if source type is known
    let src_code = types
        .get_at_exit(block_id, args[0])
        .filter(|t| t.is_single())
        .and_then(|t| {
            if t.contains(BaseType::UInt) {
                Some(1u64)
            } else if t.contains(BaseType::Int) {
                Some(2u64)
            } else if t.contains(BaseType::Float) {
                Some(3u64)
            } else {
                None
            }
        });

    if let Some(src_code) = src_code {
        // === Level 1: both source type and target known ===

        // Identity casts should have been replaced with Copy by the
        // optimizer's elide_identity_casts pass.
        debug_assert!(
            src_code != target,
            "Identity Cast (src={}, target={}) should have been elided by optimizer",
            src_code,
            target
        );

        // Fully specialized conversion — no dispatch at runtime
        return match (src_code, target) {
            (1, 2) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                vm.set_local(d, Value::Int(n as i64));
                Ok(Action::Continue)
            })),
            (1, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            (2, 1) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_int(vm, src);
                vm.set_local(d, Value::UInt(n as u64));
                Ok(Action::Continue)
            })),
            (2, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_int(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            (3, _) => Some(Box::new(move |vm: &mut VM, _| {
                vm.set_local_uninit(d);
                Ok(Action::Continue)
            })),
            _ => None,
        };
    }

    // === Level 2: target known, source type unknown ===
    // Emit a target-specific closure — eliminates target slot read and
    // target match; only source value dispatch remains.
    Some(match target {
        1 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Some(Value::UInt(*n)),
                Some(Value::Int(n)) => Some(Value::UInt(*n as u64)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        2 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Some(Value::Int(*n as i64)),
                Some(Value::Int(n)) => Some(Value::Int(*n)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        3 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Int(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Float(f)) => Some(Value::Float(*f)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        _ => return None,
    })
}

/// Try to emit a target-specialized closure for a Widen intrinsic.
///
/// Same approach as `try_specialize_cast`: the target type code is always a
/// compile-time constant. Unlike Cast, Widen is overflow-checked (UInt→Int
/// fails if value > i64::MAX).
pub(super) fn try_specialize_widen(
    op: IntrinsicOp,
    arg_slots: &[usize],
    dest_slot: usize,
    args: &[VarId],
    types: &TypeAnalysis,
    block_id: BlockId,
    consts: &HashMap<VarId, u64>,
) -> Option<Step> {
    if op != IntrinsicOp::Widen || args.len() != 2 {
        return None;
    }

    let target = *consts.get(&args[1])?;
    let src = arg_slots[0];
    let d = dest_slot;

    // Check if source type is known
    let src_code = types
        .get_at_exit(block_id, args[0])
        .filter(|t| t.is_single())
        .and_then(|t| {
            if t.contains(BaseType::UInt) {
                Some(1u64)
            } else if t.contains(BaseType::Int) {
                Some(2u64)
            } else if t.contains(BaseType::Float) {
                Some(3u64)
            } else {
                None
            }
        });

    if let Some(src_code) = src_code {
        // === Fully specialized: source type + target both known ===

        // Identity widens should have been replaced with Copy by the
        // optimizer's elide_identity_casts pass.
        debug_assert!(
            src_code != target,
            "Identity Widen (src={}, target={}) should have been elided by optimizer",
            src_code,
            target
        );

        return match (src_code, target) {
            // UInt → Int: overflow-checked
            (1, 2) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                if n > i64::MAX as u64 {
                    vm.set_local_uninit(d);
                } else {
                    vm.set_local(d, Value::Int(n as i64));
                }
                Ok(Action::Continue)
            })),
            // UInt → Float
            (1, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_uint(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            // Int → Float
            (2, 3) => Some(Box::new(move |vm: &mut VM, _| {
                let n = expect_int(vm, src);
                match Float::new(n as f64) {
                    Some(f) => vm.set_local(d, Value::Float(f)),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })),
            _ => Some(Box::new(move |vm: &mut VM, _| {
                vm.set_local_uninit(d);
                Ok(Action::Continue)
            })),
        };
    }

    // === Target known, source unknown ===
    Some(match target {
        2 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => {
                    if *n > i64::MAX as u64 {
                        None
                    } else {
                        Some(Value::Int(*n as i64))
                    }
                }
                Some(Value::Int(n)) => Some(Value::Int(*n)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        3 => Box::new(move |vm: &mut VM, _| {
            let result = match vm.local(src) {
                Some(Value::UInt(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Int(n)) => Float::new(*n as f64).map(Value::Float),
                Some(Value::Float(f)) => Some(Value::Float(*f)),
                _ => None,
            };
            match result {
                Some(v) => vm.set_local(d, v),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        _ => return None,
    })
}

// Type-extraction helpers for specialized closures.
// These use expect() rather than silent fallback — the type analysis has
// proven the types, so a mismatch is a compiler bug that should surface
// immediately during testing.

pub(super) fn expect_uint(vm: &VM, slot: usize) -> u64 {
    match vm.local(slot).expect("specialized: slot must be defined") {
        Value::UInt(n) => *n,
        other => panic!("specialized: expected UInt, got {:?}", other),
    }
}

pub(super) fn expect_int(vm: &VM, slot: usize) -> i64 {
    match vm.local(slot).expect("specialized: slot must be defined") {
        Value::Int(n) => *n,
        other => panic!("specialized: expected Int, got {:?}", other),
    }
}

pub(super) fn expect_float(vm: &VM, slot: usize) -> f64 {
    match vm.local(slot).expect("specialized: slot must be defined") {
        Value::Float(f) => f.get(),
        other => panic!("specialized: expected Float, got {:?}", other),
    }
}

/// Emit a UInt-specialized closure for a binary op.
pub(super) fn specialize_uint(op: IntrinsicOp, a: usize, b: usize, d: usize) -> Step {
    match op {
        IntrinsicOp::Add => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_add(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Sub => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_sub(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mul => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_mul(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Div => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_div(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mod => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            match x.checked_rem(y) {
                Some(r) => vm.set_local(d, Value::UInt(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Lt => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            vm.set_local(d, Value::Bool(x < y));
            Ok(Action::Continue)
        }),
        IntrinsicOp::Eq => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_uint(vm, a), expect_uint(vm, b));
            vm.set_local(d, Value::Bool(x == y));
            Ok(Action::Continue)
        }),
        _ => unreachable!(),
    }
}

/// Emit an Int-specialized closure for a binary op.
pub(super) fn specialize_int(op: IntrinsicOp, a: usize, b: usize, d: usize) -> Step {
    match op {
        IntrinsicOp::Add => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_add(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Sub => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_sub(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mul => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_mul(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Div => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_div(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mod => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            match x.checked_rem(y) {
                Some(r) => vm.set_local(d, Value::Int(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Lt => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            vm.set_local(d, Value::Bool(x < y));
            Ok(Action::Continue)
        }),
        IntrinsicOp::Eq => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_int(vm, a), expect_int(vm, b));
            vm.set_local(d, Value::Bool(x == y));
            Ok(Action::Continue)
        }),
        _ => unreachable!(),
    }
}

/// Emit a Float-specialized closure for a binary op.
pub(super) fn specialize_float(op: IntrinsicOp, a: usize, b: usize, d: usize) -> Step {
    match op {
        IntrinsicOp::Add => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x + y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Sub => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x - y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mul => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x * y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Div => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x / y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Mod => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            match Float::new(x % y) {
                Some(r) => vm.set_local(d, Value::Float(r)),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Lt => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            vm.set_local(d, Value::Bool(x < y));
            Ok(Action::Continue)
        }),
        IntrinsicOp::Eq => Box::new(move |vm: &mut VM, _| {
            let (x, y) = (expect_float(vm, a), expect_float(vm, b));
            vm.set_local(d, Value::Bool(x == y));
            Ok(Action::Continue)
        }),
        _ => unreachable!(),
    }
}

/// Compile-time dispatch: match on the IntrinsicOp and return a closure
/// specific to that operation. Eliminates the runtime `match op` that
/// `exec_intrinsic` would perform on every execution.
pub(super) fn compile_intrinsic_dispatch(
    op: IntrinsicOp,
    arg_slots: Vec<usize>,
    d: usize,
    all_defined: bool,
) -> Step {
    // Helper: wrap exec body in the standard result-to-slot pattern
    macro_rules! emit {
        ($body:expr) => {
            Box::new(move |vm: &mut VM, _prog| {
                let result: Option<Value> = $body(vm);
                match result {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        };
    }
    // Helper for operations that need ExecError propagation
    macro_rules! emit_try {
        ($body:expr) => {
            Box::new(move |vm: &mut VM, _prog| {
                match $body(vm)? {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        };
    }

    // Helpers for binary/unary ops: when all_defined, skip the Option unwrap
    // and pass &Value directly; otherwise gate on Some first.
    macro_rules! emit_binary {
        ($op_fn:ident) => {
            if all_defined {
                emit!(|vm: &mut VM| {
                    let a = vm.local(arg_slots[0]).unwrap();
                    let b = vm.local(arg_slots[1]).unwrap();
                    $op_fn(a, b)
                })
            } else {
                emit!(|vm: &mut VM| {
                    match (vm.local(arg_slots[0]), vm.local(arg_slots[1])) {
                        (Some(a), Some(b)) => $op_fn(a, b),
                        _ => None,
                    }
                })
            }
        };
    }
    macro_rules! emit_unary {
        ($op_fn:ident) => {
            if all_defined {
                emit!(|vm: &mut VM| {
                    let a = vm.local(arg_slots[0]).unwrap();
                    $op_fn(a)
                })
            } else {
                emit!(|vm: &mut VM| {
                    match vm.local(arg_slots[0]) {
                        Some(a) => $op_fn(a),
                        None => None,
                    }
                })
            }
        };
    }

    match op {
        IntrinsicOp::Add => emit_binary!(exec_add),
        IntrinsicOp::Sub => emit_binary!(exec_sub),
        IntrinsicOp::Mul => emit_binary!(exec_mul),
        IntrinsicOp::Div => emit_binary!(exec_div),
        IntrinsicOp::Mod => emit_binary!(exec_mod),
        IntrinsicOp::Neg => emit_unary!(exec_neg),
        IntrinsicOp::Eq => emit_binary!(exec_eq),
        IntrinsicOp::Lt => emit_binary!(exec_lt),
        IntrinsicOp::Not => emit_unary!(exec_not),
        IntrinsicOp::BitAnd => emit_binary!(exec_bitand),
        IntrinsicOp::BitOr => emit_binary!(exec_bitor),
        IntrinsicOp::BitXor => emit_binary!(exec_bitxor),
        IntrinsicOp::BitNot => emit_unary!(exec_bitnot),
        IntrinsicOp::Shl => emit_binary!(exec_shl),
        IntrinsicOp::Shr => emit_binary!(exec_shr),
        IntrinsicOp::BitTest => emit_binary!(exec_bittest),
        IntrinsicOp::BitSet => {
            if all_defined {
                emit!(|vm: &mut VM| {
                    let x = vm.local(arg_slots[0]).unwrap();
                    let b = vm.local(arg_slots[1]).unwrap();
                    let v = vm.local(arg_slots[2]).unwrap();
                    exec_bitset(x, b, v)
                })
            } else {
                emit!(|vm: &mut VM| {
                    match (
                        vm.local(arg_slots[0]),
                        vm.local(arg_slots[1]),
                        vm.local(arg_slots[2]),
                    ) {
                        (Some(x), Some(b), Some(v)) => exec_bitset(x, b, v),
                        _ => None,
                    }
                })
            }
        }
        IntrinsicOp::Len => emit_unary!(exec_len),
        IntrinsicOp::MakeArray => emit_try!(|vm: &mut VM| { exec_make_array(&arg_slots, vm) }),
        IntrinsicOp::MakeMap => emit_try!(|vm: &mut VM| { exec_make_map(&arg_slots, vm) }),
        IntrinsicOp::MakeSeq => emit!(|vm: &mut VM| { exec_make_seq(&arg_slots, vm) }),
        IntrinsicOp::ArraySeq => emit!(|vm: &mut VM| { exec_array_seq(&arg_slots, vm) }),
        IntrinsicOp::SeqNext => Box::new(move |vm: &mut VM, _prog| {
            match vm.seq_next(vm.bp() + arg_slots[0])? {
                Some(val) => vm.set_local(d, val),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Collect => Box::new(move |vm: &mut VM, _prog| {
            match vm.seq_collect(vm.bp() + arg_slots[0])? {
                Some(val) => vm.set_local(d, val),
                None => vm.set_local_uninit(d),
            }
            Ok(Action::Continue)
        }),
        IntrinsicOp::Widen => emit!(|vm: &mut VM| { exec_widen(&arg_slots, vm) }),
        IntrinsicOp::Cast => emit!(|vm: &mut VM| { exec_cast(&arg_slots, vm) }),
    }
}
