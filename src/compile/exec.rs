use super::*;

// ========================================================================
// Value Indexing (runtime)
// ========================================================================

pub(super) fn index_value(base: &Value, key: &Value) -> Option<Value> {
    match (base, key) {
        (Value::Array(arr), Value::UInt(idx)) => arr.get(*idx as usize).cloned(),
        (Value::Array(arr), Value::Int(idx)) if *idx >= 0 => arr.get(*idx as usize).cloned(),
        (Value::Map(map), key) => map.get(key).cloned(),
        (Value::Text(s), Value::UInt(idx)) => {
            // Text indexing returns UInt (Unicode code point). No Char type —
            // UInt serves as the character representation, keeping the type system small.
            s.chars().nth(*idx as usize).map(|c| Value::UInt(c as u64))
        }
        (Value::Bytes(b), Value::UInt(idx)) => {
            b.get(*idx as usize).map(|byte| Value::UInt(*byte as u64))
        }
        _ => None,
    }
}

// ========================================================================
// Per-operation functions for compile-time dispatch
// Each takes &Value directly (no Option wrapper, no slot lookup, no op dispatch).
// The Option handling is done at the call site in compile_intrinsic_dispatch:
// - all_defined=true: unwrap() then call (skips None check entirely)
// - all_defined=false: gate on Some first, call only if all present
// ========================================================================

pub(super) fn exec_add(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_add(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_add(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() + b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_add(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_add(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 + b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() + *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 + b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() + *b as f64).map(Value::Float),
        _ => None,
    }
}

pub(super) fn exec_sub(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_sub(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_sub(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() - b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_sub(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_sub(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 - b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() - *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 - b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() - *b as f64).map(Value::Float),
        _ => None,
    }
}

pub(super) fn exec_mul(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_mul(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_mul(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() * b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_mul(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_mul(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 * b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() * *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 * b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() * *b as f64).map(Value::Float),
        _ => None,
    }
}

pub(super) fn exec_div(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_div(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_div(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() / b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_div(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_div(b))
            .map(Value::Int),
        (Value::UInt(a), Value::Float(b)) => Float::new(*a as f64 / b.get()).map(Value::Float),
        (Value::Float(a), Value::UInt(b)) => Float::new(a.get() / *b as f64).map(Value::Float),
        (Value::Int(a), Value::Float(b)) => Float::new(*a as f64 / b.get()).map(Value::Float),
        (Value::Float(a), Value::Int(b)) => Float::new(a.get() / *b as f64).map(Value::Float),
        _ => None,
    }
}

pub(super) fn exec_mod(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => a.checked_rem(*b).map(Value::UInt),
        (Value::Int(a), Value::Int(b)) => a.checked_rem(*b).map(Value::Int),
        (Value::Float(a), Value::Float(b)) => Float::new(a.get() % b.get()).map(Value::Float),
        (Value::UInt(a), Value::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_rem(*b))
            .map(Value::Int),
        (Value::Int(a), Value::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_rem(b))
            .map(Value::Int),
        _ => None,
    }
}

pub(super) fn exec_neg(a: &Value) -> Option<Value> {
    match a {
        Value::Int(a) => a.checked_neg().map(Value::Int),
        Value::Float(a) => Float::new(-a.get()).map(Value::Float),
        Value::UInt(a) => i64::try_from(*a)
            .ok()
            .and_then(|v| v.checked_neg())
            .map(Value::Int),
        _ => None,
    }
}

pub(super) fn exec_eq(a: &Value, b: &Value) -> Option<Value> {
    Some(Value::Bool(a == b))
}

pub(super) fn exec_lt(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::Bool(a < b)),
        (Value::Int(a), Value::Int(b)) => Some(Value::Bool(a < b)),
        (Value::Float(a), Value::Float(b)) => Some(Value::Bool(a.get() < b.get())),
        (Value::UInt(a), Value::Int(b)) => Some(Value::Bool((*a as i128) < (*b as i128))),
        (Value::Int(a), Value::UInt(b)) => Some(Value::Bool((*a as i128) < (*b as i128))),
        (Value::UInt(a), Value::Float(b)) => Some(Value::Bool((*a as f64) < b.get())),
        (Value::Float(a), Value::UInt(b)) => Some(Value::Bool(a.get() < (*b as f64))),
        (Value::Int(a), Value::Float(b)) => Some(Value::Bool((*a as f64) < b.get())),
        (Value::Float(a), Value::Int(b)) => Some(Value::Bool(a.get() < (*b as f64))),
        _ => None,
    }
}

pub(super) fn exec_not(a: &Value) -> Option<Value> {
    match a {
        Value::Bool(b) => Some(Value::Bool(!b)),
        _ => None,
    }
}

pub(super) fn exec_bitand(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a & b)),
        _ => None,
    }
}

pub(super) fn exec_bitor(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a | b)),
        _ => None,
    }
}

pub(super) fn exec_bitxor(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a ^ b)),
        _ => None,
    }
}

pub(super) fn exec_bitnot(a: &Value) -> Option<Value> {
    match a {
        Value::UInt(a) => Some(Value::UInt(!a)),
        _ => None,
    }
}

pub(super) fn exec_shl(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a.wrapping_shl(*b as u32))),
        _ => None,
    }
}

pub(super) fn exec_shr(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::UInt(a), Value::UInt(b)) => Some(Value::UInt(a.wrapping_shr(*b as u32))),
        _ => None,
    }
}

pub(super) fn exec_bittest(x: &Value, b: &Value) -> Option<Value> {
    match (x, b) {
        (Value::UInt(x), Value::UInt(b)) => {
            if *b >= 64 {
                None
            } else {
                Some(Value::Bool((x >> b) & 1 == 1))
            }
        }
        _ => None,
    }
}

pub(super) fn exec_bitset(x: &Value, b: &Value, v: &Value) -> Option<Value> {
    match (x, b, v) {
        (Value::UInt(x), Value::UInt(b), Value::Bool(v)) => {
            if *b >= 64 {
                None
            } else if *v {
                Some(Value::UInt(x | (1 << b)))
            } else {
                Some(Value::UInt(x & !(1 << b)))
            }
        }
        _ => None,
    }
}

pub(super) fn exec_len(a: &Value) -> Option<Value> {
    match a {
        Value::Text(s) => Some(Value::UInt(s.chars().count() as u64)),
        Value::Bytes(b) => Some(Value::UInt(b.len() as u64)),
        Value::Array(arr) => Some(Value::UInt(arr.len() as u64)),
        Value::Map(map) => Some(Value::UInt(map.len() as u64)),
        Value::Sequence(seq) => seq.remaining().map(|n| Value::UInt(n as u64)),
        _ => None,
    }
}

pub(super) fn exec_make_array(
    arg_slots: &[usize],
    vm: &mut VM,
) -> Result<Option<Value>, ExecError> {
    let elems: Vec<Value> = arg_slots
        .iter()
        .filter_map(|s| vm.local(*s).cloned())
        .collect();
    let arr = HeapVal::new(elems, vm.heap())?;
    Ok(Some(Value::Array(arr)))
}

pub(super) fn exec_make_map(arg_slots: &[usize], vm: &mut VM) -> Result<Option<Value>, ExecError> {
    if !arg_slots.len().is_multiple_of(2) {
        return Ok(None);
    }
    let map: IndexMap<Value, Value> = arg_slots
        .chunks(2)
        .filter_map(|pair| {
            let k = vm.local(pair[0]).cloned()?;
            let v = vm.local(pair[1]).cloned()?;
            Some((k, v))
        })
        .collect();
    let heap_map = HeapVal::new(map, vm.heap())?;
    Ok(Some(Value::Map(heap_map)))
}

pub(super) fn exec_make_seq(arg_slots: &[usize], vm: &mut VM) -> Option<Value> {
    let (start, end) = match (vm.local(arg_slots[0]), vm.local(arg_slots[1])) {
        (Some(Value::UInt(s)), Some(Value::UInt(e))) => (*s, *e),
        _ => return None,
    };
    let state = SeqState::Range {
        current: start,
        end,
    };
    HeapVal::new(state, vm.heap()).ok().map(Value::Sequence)
}

pub(super) fn exec_array_seq(arg_slots: &[usize], vm: &mut VM) -> Option<Value> {
    let start = match vm.local(arg_slots[1]) {
        Some(Value::UInt(n)) => *n as usize,
        _ => return None,
    };
    let end = match vm.local(arg_slots[2]) {
        Some(Value::UInt(n)) => *n as usize,
        _ => return None,
    };
    match vm.local(arg_slots[0]) {
        Some(Value::Array(arr)) => {
            let state = SeqState::ArraySlice {
                source: arr.clone(),
                start,
                end,
            };
            HeapVal::new(state, vm.heap()).ok().map(Value::Sequence)
        }
        _ => None,
    }
}

pub(super) fn exec_convert(
    target: NumericType,
    mode: ConvertMode,
    arg_slots: &[usize],
    vm: &VM,
) -> Option<Value> {
    let value = vm.local(arg_slots[0]);
    match (value, target, mode) {
        // Identity
        (Some(Value::UInt(n)), NumericType::UInt, _) => Some(Value::UInt(*n)),
        (Some(Value::Int(n)), NumericType::Int, _) => Some(Value::Int(*n)),
        (Some(Value::Float(f)), NumericType::Float, _) => Some(Value::Float(*f)),
        // UInt → Int: checked overflows, unchecked wraps
        (Some(Value::UInt(n)), NumericType::Int, ConvertMode::Checked) => {
            if *n > i64::MAX as u64 {
                None
            } else {
                Some(Value::Int(*n as i64))
            }
        }
        (Some(Value::UInt(n)), NumericType::Int, ConvertMode::Unchecked) => {
            Some(Value::Int(*n as i64))
        }
        // Int → UInt: unchecked only (bit reinterpret)
        (Some(Value::Int(n)), NumericType::UInt, ConvertMode::Unchecked) => {
            Some(Value::UInt(*n as u64))
        }
        // → Float: same for both modes
        (Some(Value::UInt(n)), NumericType::Float, _) => Float::new(*n as f64).map(Value::Float),
        (Some(Value::Int(n)), NumericType::Float, _) => Float::new(*n as f64).map(Value::Float),
        _ => None,
    }
}

// ============================================================================
// Tests
// ============================================================================
