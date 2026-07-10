use super::*;

// ========================================================================
// Value Indexing (runtime)
// ========================================================================

pub(super) fn index_value(base: &Value, key: &Value) -> Value {
    match (base, key) {
        (Value::Array(arr), Value::UInt(idx)) => {
            arr.get(*idx as usize).cloned().unwrap_or(Value::Undefined)
        }
        (Value::Array(arr), Value::Int(idx)) if *idx >= 0 => {
            arr.get(*idx as usize).cloned().unwrap_or(Value::Undefined)
        }
        (Value::Map(map), key) => map.get(key).cloned().unwrap_or(Value::Undefined),
        (Value::Text(s), Value::UInt(idx)) => s
            .chars()
            .nth(*idx as usize)
            .map_or(Value::Undefined, |c| Value::UInt(c as u64)),
        (Value::Bytes(b), Value::UInt(idx)) => b
            .get(*idx as usize)
            .map_or(Value::Undefined, |byte| Value::UInt(*byte as u64)),
        _ => Value::Undefined,
    }
}

// ========================================================================
// Per-operation functions for compile-time dispatch
// Each takes &Value directly (no slot lookup, no op dispatch).
// Returns Value::Undefined for type mismatches or domain errors.
//
// Functions that are only reached from guarded paths (binary/unary operators
// wrapped in lower_guarded_expression) have debug_assert! on their catch-all
// arms to validate guard coverage. When the StepKind peephole layer fuses
// Match + Op into single typed closures, these asserts must be reverted to
// plain Value::Undefined — the fused path handles dispatch internally, and
// the unfused fallback becomes reachable for unguarded contexts.
// ========================================================================

pub(super) fn exec_add(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_add(*b).map_or(Value::Undefined, Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_add(*b).map_or(Value::Undefined, Value::Int),
        (Value::Float(a), Value::Float(b)) => {
            Float::new(a.get() + b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_add(*b))
            .map_or(Value::Undefined, Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_add(b))
            .map_or(Value::Undefined, Value::Int),
        (Value::UInt(a), Value::Float(b)) => {
            Float::new(*a as f64 + b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::UInt(b)) => {
            Float::new(a.get() + *b as f64).map_or(Value::Undefined, Value::Float)
        }
        (Value::Int(a), Value::Float(b)) => {
            Float::new(*a as f64 + b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::Int(b)) => {
            Float::new(a.get() + *b as f64).map_or(Value::Undefined, Value::Float)
        }
        _ => Value::Undefined,
    }
}

pub(super) fn exec_sub(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_sub(*b).map_or(Value::Undefined, Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_sub(*b).map_or(Value::Undefined, Value::Int),
        (Value::Float(a), Value::Float(b)) => {
            Float::new(a.get() - b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_sub(*b))
            .map_or(Value::Undefined, Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_sub(b))
            .map_or(Value::Undefined, Value::Int),
        (Value::UInt(a), Value::Float(b)) => {
            Float::new(*a as f64 - b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::UInt(b)) => {
            Float::new(a.get() - *b as f64).map_or(Value::Undefined, Value::Float)
        }
        (Value::Int(a), Value::Float(b)) => {
            Float::new(*a as f64 - b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::Int(b)) => {
            Float::new(a.get() - *b as f64).map_or(Value::Undefined, Value::Float)
        }
        _ => Value::Undefined,
    }
}

pub(super) fn exec_mul(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_mul(*b).map_or(Value::Undefined, Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_mul(*b).map_or(Value::Undefined, Value::Int),
        (Value::Float(a), Value::Float(b)) => {
            Float::new(a.get() * b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_mul(*b))
            .map_or(Value::Undefined, Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_mul(b))
            .map_or(Value::Undefined, Value::Int),
        (Value::UInt(a), Value::Float(b)) => {
            Float::new(*a as f64 * b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::UInt(b)) => {
            Float::new(a.get() * *b as f64).map_or(Value::Undefined, Value::Float)
        }
        (Value::Int(a), Value::Float(b)) => {
            Float::new(*a as f64 * b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::Int(b)) => {
            Float::new(a.get() * *b as f64).map_or(Value::Undefined, Value::Float)
        }
        _ => {
            debug_assert!(
                false,
                "exec_mul: type guard should have rejected non-numeric types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_div(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_div(*b).map_or(Value::Undefined, Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_div(*b).map_or(Value::Undefined, Value::Int),
        (Value::Float(a), Value::Float(b)) => {
            Float::new(a.get() / b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_div(*b))
            .map_or(Value::Undefined, Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_div(b))
            .map_or(Value::Undefined, Value::Int),
        (Value::UInt(a), Value::Float(b)) => {
            Float::new(*a as f64 / b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::UInt(b)) => {
            Float::new(a.get() / *b as f64).map_or(Value::Undefined, Value::Float)
        }
        (Value::Int(a), Value::Float(b)) => {
            Float::new(*a as f64 / b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::Float(a), Value::Int(b)) => {
            Float::new(a.get() / *b as f64).map_or(Value::Undefined, Value::Float)
        }
        _ => {
            debug_assert!(
                false,
                "exec_div: type guard should have rejected non-numeric types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_mod(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_rem(*b).map_or(Value::Undefined, Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_rem(*b).map_or(Value::Undefined, Value::Int),
        (Value::Float(a), Value::Float(b)) => {
            Float::new(a.get() % b.get()).map_or(Value::Undefined, Value::Float)
        }
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_rem(*b))
            .map_or(Value::Undefined, Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_rem(b))
            .map_or(Value::Undefined, Value::Int),
        _ => {
            debug_assert!(
                false,
                "exec_mod: type guard should have rejected non-numeric types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_neg(a: &Value) -> Value {
    match a {
        Value::Int(a) => a.checked_neg().map_or(Value::Undefined, Value::Int),
        Value::Float(a) => Float::new(-a.get()).map_or(Value::Undefined, Value::Float),
        Value::UInt(a) => i64::try_from(*a)
            .ok()
            .and_then(|v| v.checked_neg())
            .map_or(Value::Undefined, Value::Int),
        _ => {
            debug_assert!(
                false,
                "exec_neg: type guard should have rejected non-numeric types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_eq(a: &Value, b: &Value) -> Value {
    if a.is_undefined() || b.is_undefined() {
        debug_assert!(
            false,
            "exec_eq: type guard should have rejected Undefined values"
        );
        return Value::Undefined;
    }
    Value::Bool(a == b)
}

pub(super) fn exec_lt(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Value::Bool(a < b),
        (Value::Int(a), Value::Int(b)) => Value::Bool(a < b),
        (Value::Float(a), Value::Float(b)) => Value::Bool(a.get() < b.get()),
        (Value::UInt(a), Value::Int(b)) => Value::Bool((*a as i128) < (*b as i128)),
        (Value::Int(a), Value::UInt(b)) => Value::Bool((*a as i128) < (*b as i128)),
        (Value::UInt(a), Value::Float(b)) => Value::Bool((*a as f64) < b.get()),
        (Value::Float(a), Value::UInt(b)) => Value::Bool(a.get() < (*b as f64)),
        (Value::Int(a), Value::Float(b)) => Value::Bool((*a as f64) < b.get()),
        (Value::Float(a), Value::Int(b)) => Value::Bool(a.get() < (*b as f64)),
        _ => Value::Undefined,
    }
}

pub(super) fn exec_not(a: &Value) -> Value {
    match a {
        Value::Bool(b) => Value::Bool(!b),
        _ => {
            debug_assert!(
                false,
                "exec_not: type guard should have rejected non-Bool type"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_bitand(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Value::UInt(a & b),
        _ => {
            debug_assert!(
                false,
                "exec_bitand: type guard should have rejected non-UInt types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_bitor(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Value::UInt(a | b),
        _ => {
            debug_assert!(
                false,
                "exec_bitor: type guard should have rejected non-UInt types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_bitxor(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Value::UInt(a ^ b),
        _ => {
            debug_assert!(
                false,
                "exec_bitxor: type guard should have rejected non-UInt types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_bitnot(a: &Value) -> Value {
    match a {
        Value::UInt(a) => Value::UInt(!a),
        _ => {
            debug_assert!(
                false,
                "exec_bitnot: type guard should have rejected non-UInt type"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_shl(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Value::UInt(a.wrapping_shl(*b as u32)),
        _ => {
            debug_assert!(
                false,
                "exec_shl: type guard should have rejected non-UInt types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_shr(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Value::UInt(a.wrapping_shr(*b as u32)),
        _ => {
            debug_assert!(
                false,
                "exec_shr: type guard should have rejected non-UInt types"
            );
            Value::Undefined
        }
    }
}

pub(super) fn exec_bittest(x: &Value, b: &Value) -> Value {
    match (x, b) {
        (Value::UInt(x), Value::UInt(b)) => {
            if *b >= 64 {
                Value::Undefined
            } else {
                Value::Bool((x >> b) & 1 == 1)
            }
        }
        _ => Value::Undefined,
    }
}

pub(super) fn exec_bitset(x: &Value, b: &Value, v: &Value) -> Value {
    match (x, b, v) {
        (Value::UInt(x), Value::UInt(b), Value::Bool(v)) => {
            if *b >= 64 {
                Value::Undefined
            } else if *v {
                Value::UInt(x | (1 << b))
            } else {
                Value::UInt(x & !(1 << b))
            }
        }
        _ => Value::Undefined,
    }
}

pub(super) fn exec_len(a: &Value) -> Value {
    match a {
        Value::Text(s) => Value::UInt(s.chars().count() as u64),
        Value::Bytes(b) => Value::UInt(b.len() as u64),
        Value::Array(arr) => Value::UInt(arr.len() as u64),
        Value::Map(map) => Value::UInt(map.len() as u64),
        Value::Sequence(seq) => seq
            .remaining()
            .map_or(Value::Undefined, |n| Value::UInt(n as u64)),
        _ => Value::Undefined,
    }
}

/// The i-th key of a Map in insertion order (`IndexMap` is ordered). Used by
/// `for k, v in map` lowering. Out-of-range → Undefined (the loop keeps `i < len`).
pub(super) fn exec_map_key_at(map: &Value, index: &Value) -> Value {
    match (map, index) {
        (Value::Map(m), Value::UInt(i)) => m
            .get_index(*i as usize)
            .map(|(k, _)| k.clone())
            .unwrap_or(Value::Undefined),
        _ => Value::Undefined,
    }
}

pub(super) fn exec_make_array(arg_slots: &[usize], vm: &mut VM) -> Result<Value, ExecError> {
    let elems: Vec<Value> = arg_slots
        .iter()
        .map(|s| vm.local(*s).clone())
        .filter(|v| v.is_defined())
        .collect();
    let arr = HeapVal::new(elems, vm.heap())?;
    Ok(Value::Array(arr))
}

pub(super) fn exec_make_map(arg_slots: &[usize], vm: &mut VM) -> Result<Value, ExecError> {
    debug_assert!(
        arg_slots.len().is_multiple_of(2),
        "MakeMap: odd arg count {} is a compiler bug",
        arg_slots.len()
    );
    let map: IndexMap<Value, Value> = arg_slots
        .chunks(2)
        .filter_map(|pair| {
            let k = vm.local(pair[0]).clone();
            let v = vm.local(pair[1]).clone();
            if k.is_defined() && v.is_defined() {
                Some((k, v))
            } else {
                None
            }
        })
        .collect();
    let heap_map = HeapVal::new(map, vm.heap())?;
    Ok(Value::Map(heap_map))
}

pub(super) fn exec_make_seq(arg_slots: &[usize], vm: &mut VM) -> Value {
    let (start, end) = match (vm.local(arg_slots[0]), vm.local(arg_slots[1])) {
        (Value::UInt(s), Value::UInt(e)) => (*s, *e),
        _ => return Value::Undefined,
    };
    let state = SeqState::Range {
        current: start,
        end,
    };
    HeapVal::new(state, vm.heap())
        .ok()
        .map_or(Value::Undefined, Value::Sequence)
}

pub(super) fn exec_array_seq(arg_slots: &[usize], vm: &mut VM) -> Value {
    let start = match vm.local(arg_slots[1]) {
        Value::UInt(n) => *n as usize,
        _ => return Value::Undefined,
    };
    let end = match vm.local(arg_slots[2]) {
        Value::UInt(n) => *n as usize,
        _ => return Value::Undefined,
    };
    match vm.local(arg_slots[0]) {
        Value::Array(arr) => {
            let state = SeqState::ArraySlice {
                source: arr.clone(),
                start,
                end,
            };
            HeapVal::new(state, vm.heap())
                .ok()
                .map_or(Value::Undefined, Value::Sequence)
        }
        _ => Value::Undefined,
    }
}

pub(super) fn exec_convert(
    target: NumericType,
    mode: ConvertMode,
    arg_slots: &[usize],
    vm: &VM,
) -> Value {
    let value = vm.local(arg_slots[0]);
    match (value, target, mode) {
        // Identity
        (Value::UInt(n), NumericType::UInt, _) => Value::UInt(*n),
        (Value::Int(n), NumericType::Int, _) => Value::Int(*n),
        (Value::Float(f), NumericType::Float, _) => Value::Float(*f),
        // UInt → Int: checked overflows, unchecked wraps
        (Value::UInt(n), NumericType::Int, ConvertMode::Checked) => {
            if *n > i64::MAX as u64 {
                Value::Undefined
            } else {
                Value::Int(*n as i64)
            }
        }
        (Value::UInt(n), NumericType::Int, ConvertMode::Unchecked) => Value::Int(*n as i64),
        // Int → UInt: unchecked only (bit reinterpret)
        (Value::Int(n), NumericType::UInt, ConvertMode::Unchecked) => Value::UInt(*n as u64),
        // → Float: same for both modes
        (Value::UInt(n), NumericType::Float, _) => {
            Float::new(*n as f64).map_or(Value::Undefined, Value::Float)
        }
        (Value::Int(n), NumericType::Float, _) => {
            Float::new(*n as f64).map_or(Value::Undefined, Value::Float)
        }
        _ => Value::Undefined,
    }
}

// ============================================================================
// Tests
// ============================================================================
