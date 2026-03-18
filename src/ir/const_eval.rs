//! Shared Constant Evaluation Utilities
//!
//! Common helpers for compile-time evaluation, used by both:
//! - `constant.rs`: Evaluating `const` declarations during lowering
//! - `opt/const_fold.rs`: Constant folding optimization pass
//!
//! # Future: Const User Functions
//!
//! Currently only builtin functions with `Purity::Const` can be evaluated at
//! compile time. In the future, we could support const user functions by:
//! 1. Inferring or annotating pure functions
//! 2. Interpreting the IR with constant inputs
//! 3. Detecting and preventing side effects

use crate::builtins::BuiltinRegistry;
use crate::ir::{ConstValue, FunctionRef, Literal};

// ============================================================================
// Literal <-> ConstValue Conversion
// ============================================================================

/// Convert an IR Literal to a ConstValue
pub fn literal_to_const(lit: &Literal) -> ConstValue {
    match lit {
        Literal::Bool(b) => ConstValue::Bool(*b),
        Literal::UInt(n) => ConstValue::UInt(*n),
        Literal::Int(n) => ConstValue::Int(*n),
        Literal::Float(f) => ConstValue::Float(*f),
        Literal::Text(s) => ConstValue::Text(s.clone()),
        Literal::Bytes(b) => ConstValue::Bytes(b.clone()),
    }
}

/// Convert a ConstValue to an IR Literal
///
/// Returns `None` for compound types (Array, Map) which can't be represented
/// as a single Literal instruction.
pub fn const_to_literal(cv: &ConstValue) -> Option<Literal> {
    Some(match cv {
        ConstValue::Bool(b) => Literal::Bool(*b),
        ConstValue::UInt(n) => Literal::UInt(*n),
        ConstValue::Int(n) => Literal::Int(*n),
        ConstValue::Float(f) => Literal::Float(*f),
        ConstValue::Text(s) => Literal::Text(s.clone()),
        ConstValue::Bytes(b) => Literal::Bytes(b.clone()),
        // Compound types can't be represented as Literal
        ConstValue::Array(_) | ConstValue::Map(_) => return None,
    })
}

// ============================================================================
// Builtin Const Evaluation
// ============================================================================

/// Evaluate a builtin function call with constant arguments
///
/// Returns `None` if:
/// - The function is not found in the registry
/// - The function doesn't have a const evaluator
/// - The const evaluation fails (e.g., domain error)
pub fn eval_builtin_const(
    func: &FunctionRef,
    args: &[ConstValue],
    registry: &BuiltinRegistry,
) -> Option<ConstValue> {
    let builtin = registry.lookup(func)?;
    let eval_fn = builtin.meta.purity.const_eval()?;
    eval_fn(args)
}

// ============================================================================
// Collection Indexing
// ============================================================================

/// Index into a constant collection (array, text, map)
///
/// Returns `None` if:
/// - Index is out of bounds
/// - Key is not found in map
/// - Types are incompatible for indexing
pub fn const_index(base: &ConstValue, key: &ConstValue) -> Option<ConstValue> {
    match (base, key) {
        // Array[UInt]
        (ConstValue::Array(arr), ConstValue::UInt(idx)) => arr.get(*idx as usize).cloned(),
        // Array[Int] (if non-negative)
        (ConstValue::Array(arr), ConstValue::Int(idx)) => {
            if *idx >= 0 {
                arr.get(*idx as usize).cloned()
            } else {
                None
            }
        }
        // Text[UInt] -> Unicode code point as UInt (no Char type)
        (ConstValue::Text(s), ConstValue::UInt(idx)) => s
            .chars()
            .nth(*idx as usize)
            .map(|c| ConstValue::UInt(c as u64)),
        // Text[Int] (if non-negative)
        (ConstValue::Text(s), ConstValue::Int(idx)) => {
            if *idx >= 0 {
                s.chars()
                    .nth(*idx as usize)
                    .map(|c| ConstValue::UInt(c as u64))
            } else {
                None
            }
        }
        // Map[key]
        (ConstValue::Map(pairs), key) => {
            pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        }
        // Bytes[UInt] -> single byte as UInt
        (ConstValue::Bytes(bytes), ConstValue::UInt(idx)) => bytes
            .get(*idx as usize)
            .map(|b| ConstValue::UInt(*b as u64)),
        _ => None,
    }
}

// ============================================================================
// Intrinsic Const Evaluation
// ============================================================================

/// Evaluate an intrinsic operation at compile time with constant arguments.
///
/// This replaces the old approach of looking up const-eval functions from the
/// BuiltinRegistry. All operator semantics are defined here, in one place.
pub fn eval_intrinsic_const(op: crate::ir::IntrinsicOp, args: &[ConstValue]) -> Option<ConstValue> {
    use crate::ir::IntrinsicOp;
    match op {
        // -- Arithmetic --
        IntrinsicOp::Add => const_add(args),
        IntrinsicOp::Sub => const_sub(args),
        IntrinsicOp::Mul => const_mul(args),
        IntrinsicOp::Div => const_div(args),
        IntrinsicOp::Mod => const_mod(args),
        IntrinsicOp::Neg => const_neg(args),

        // -- Comparison --
        IntrinsicOp::Eq => const_eq(args),
        IntrinsicOp::Lt => const_lt(args),

        // -- Logical --
        // Note: && and || lower to control flow, not Intrinsic instructions.
        IntrinsicOp::Not => const_not(args),

        // -- Bitwise --
        IntrinsicOp::BitAnd => const_bit_and(args),
        IntrinsicOp::BitOr => const_bit_or(args),
        IntrinsicOp::BitXor => const_bit_xor(args),
        IntrinsicOp::BitNot => const_bit_not(args),
        IntrinsicOp::Shl => const_shl(args),
        IntrinsicOp::Shr => const_shr(args),
        IntrinsicOp::BitTest => const_bit_test(args),
        IntrinsicOp::BitSet => const_bit_set(args),

        // -- Collection --
        IntrinsicOp::Len => const_len(args),
        IntrinsicOp::MakeArray => Some(ConstValue::Array(args.to_vec())),
        IntrinsicOp::MakeMap => {
            if !args.len().is_multiple_of(2) {
                return None;
            }
            let pairs = args
                .chunks(2)
                .map(|c| (c[0].clone(), c[1].clone()))
                .collect();
            Some(ConstValue::Map(pairs))
        }

        // Sequences are runtime-only (lazy), can't const-eval
        IntrinsicOp::MakeSeq
        | IntrinsicOp::ArraySeq
        | IntrinsicOp::SeqNext
        | IntrinsicOp::Collect => None,

        // Widen: numeric coercion along the promotion lattice
        IntrinsicOp::Widen => {
            let value = args.first()?;
            let target = match args.get(1)? {
                ConstValue::UInt(t) => *t,
                _ => return None,
            };
            match (value, target) {
                // Target = Int (BaseType::Int = 2)
                (ConstValue::UInt(n), 2) => {
                    let n = *n;
                    if n > i64::MAX as u64 {
                        None // overflow
                    } else {
                        Some(ConstValue::Int(n as i64))
                    }
                }
                (ConstValue::Int(n), 2) => Some(ConstValue::Int(*n)), // identity
                // Target = Float (BaseType::Float = 3)
                (ConstValue::UInt(n), 3) => Some(ConstValue::Float(*n as f64)),
                (ConstValue::Int(n), 3) => Some(ConstValue::Float(*n as f64)),
                (ConstValue::Float(f), 3) => Some(ConstValue::Float(*f)), // identity
                _ => None,
            }
        }

        // Cast: infallible numeric reinterpretation / widening
        IntrinsicOp::Cast => {
            let value = args.first()?;
            let target = match args.get(1)? {
                ConstValue::UInt(t) => *t,
                _ => return None,
            };
            match (value, target) {
                // Target = UInt (1)
                (ConstValue::UInt(n), 1) => Some(ConstValue::UInt(*n)),
                (ConstValue::Int(n), 1) => Some(ConstValue::UInt(*n as u64)), // bit reinterpret
                // Target = Int (2)
                (ConstValue::UInt(n), 2) => Some(ConstValue::Int(*n as i64)), // bit reinterpret
                (ConstValue::Int(n), 2) => Some(ConstValue::Int(*n)),
                // Target = Float (3)
                (ConstValue::UInt(n), 3) => Some(ConstValue::Float(*n as f64)),
                (ConstValue::Int(n), 3) => Some(ConstValue::Float(*n as f64)),
                (ConstValue::Float(f), 3) => Some(ConstValue::Float(*f)),
                _ => None,
            }
        }
    }
}

// -- Arithmetic const-eval --

fn const_add(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_add(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_add(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a + b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_add(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_add(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 + b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a + *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 + b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a + *b as f64)),
        _ => None,
    }
}

fn const_sub(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_sub(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_sub(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a - b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_sub(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_sub(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 - b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a - *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 - b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a - *b as f64)),
        _ => None,
    }
}

fn const_mul(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_mul(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_mul(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a * b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_mul(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_mul(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 * b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a * *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 * b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a * *b as f64)),
        _ => None,
    }
}

fn const_div(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_div(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_div(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a / b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_div(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_div(b))
            .map(ConstValue::Int),
        (ConstValue::UInt(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 / b)),
        (ConstValue::Float(a), ConstValue::UInt(b)) => Some(ConstValue::Float(a / *b as f64)),
        (ConstValue::Int(a), ConstValue::Float(b)) => Some(ConstValue::Float(*a as f64 / b)),
        (ConstValue::Float(a), ConstValue::Int(b)) => Some(ConstValue::Float(a / *b as f64)),
        _ => None,
    }
}

fn const_mod(args: &[ConstValue]) -> Option<ConstValue> {
    match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a.checked_rem(*b).map(ConstValue::UInt),
        (ConstValue::Int(a), ConstValue::Int(b)) => a.checked_rem(*b).map(ConstValue::Int),
        (ConstValue::Float(a), ConstValue::Float(b)) => Some(ConstValue::Float(a % b)),
        (ConstValue::UInt(a), ConstValue::Int(b)) => i64::try_from(*a)
            .ok()
            .and_then(|a| a.checked_rem(*b))
            .map(ConstValue::Int),
        (ConstValue::Int(a), ConstValue::UInt(b)) => i64::try_from(*b)
            .ok()
            .and_then(|b| a.checked_rem(b))
            .map(ConstValue::Int),
        _ => None,
    }
}

fn const_neg(args: &[ConstValue]) -> Option<ConstValue> {
    match args.first()? {
        ConstValue::Int(a) => a.checked_neg().map(ConstValue::Int),
        ConstValue::Float(a) => Some(ConstValue::Float(-a)),
        ConstValue::UInt(a) => i64::try_from(*a)
            .ok()
            .and_then(|v| v.checked_neg())
            .map(ConstValue::Int),
        _ => None,
    }
}

// -- Comparison const-eval --

fn const_eq(args: &[ConstValue]) -> Option<ConstValue> {
    Some(ConstValue::Bool(args.first()? == args.get(1)?))
}

fn const_lt(args: &[ConstValue]) -> Option<ConstValue> {
    let result = match (args.first()?, args.get(1)?) {
        (ConstValue::UInt(a), ConstValue::UInt(b)) => a < b,
        (ConstValue::Int(a), ConstValue::Int(b)) => a < b,
        (ConstValue::Float(a), ConstValue::Float(b)) => a < b,
        (ConstValue::UInt(a), ConstValue::Int(b)) => (*a as i128) < (*b as i128),
        (ConstValue::Int(a), ConstValue::UInt(b)) => (*a as i128) < (*b as i128),
        (ConstValue::UInt(a), ConstValue::Float(b)) => (*a as f64) < *b,
        (ConstValue::Float(a), ConstValue::UInt(b)) => *a < (*b as f64),
        (ConstValue::Int(a), ConstValue::Float(b)) => (*a as f64) < *b,
        (ConstValue::Float(a), ConstValue::Int(b)) => *a < (*b as f64),
        _ => return None,
    };
    Some(ConstValue::Bool(result))
}

// -- Logical const-eval --

fn const_not(args: &[ConstValue]) -> Option<ConstValue> {
    if let Some(ConstValue::Bool(a)) = args.first() {
        Some(ConstValue::Bool(!a))
    } else {
        None
    }
}

// -- Bitwise const-eval --

fn const_bit_and(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a & b))
    } else {
        None
    }
}

fn const_bit_or(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a | b))
    } else {
        None
    }
}

fn const_bit_xor(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a ^ b))
    } else {
        None
    }
}

fn const_bit_not(args: &[ConstValue]) -> Option<ConstValue> {
    if let Some(ConstValue::UInt(a)) = args.first() {
        Some(ConstValue::UInt(!a))
    } else {
        None
    }
}

fn const_shl(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a.wrapping_shl(*b as u32)))
    } else {
        None
    }
}

fn const_shr(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(a)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        Some(ConstValue::UInt(a.wrapping_shr(*b as u32)))
    } else {
        None
    }
}

fn const_bit_test(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(x)), Some(ConstValue::UInt(b))) = (args.first(), args.get(1)) {
        if *b >= 64 {
            None
        } else {
            Some(ConstValue::Bool((x >> b) & 1 == 1))
        }
    } else {
        None
    }
}

fn const_bit_set(args: &[ConstValue]) -> Option<ConstValue> {
    if let (Some(ConstValue::UInt(x)), Some(ConstValue::UInt(b)), Some(ConstValue::Bool(v))) =
        (args.first(), args.get(1), args.get(2))
    {
        if *b >= 64 {
            None
        } else if *v {
            Some(ConstValue::UInt(x | (1 << b)))
        } else {
            Some(ConstValue::UInt(x & !(1 << b)))
        }
    } else {
        None
    }
}

// -- Collection const-eval --

fn const_len(args: &[ConstValue]) -> Option<ConstValue> {
    let len = match args.first()? {
        ConstValue::Text(s) => s.chars().count() as u64,
        ConstValue::Bytes(b) => b.len() as u64,
        ConstValue::Array(arr) => arr.len() as u64,
        ConstValue::Map(map) => map.len() as u64,
        _ => return None,
    };
    Some(ConstValue::UInt(len))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::IntrinsicOp;

    #[test]
    fn test_literal_to_const() {
        assert_eq!(
            literal_to_const(&Literal::Bool(true)),
            ConstValue::Bool(true)
        );
        assert_eq!(literal_to_const(&Literal::UInt(42)), ConstValue::UInt(42));
        assert_eq!(literal_to_const(&Literal::Int(-5)), ConstValue::Int(-5));
        assert_eq!(
            literal_to_const(&Literal::Text("hello".to_string())),
            ConstValue::Text("hello".to_string())
        );
    }

    #[test]
    fn test_const_to_literal() {
        assert_eq!(
            const_to_literal(&ConstValue::Bool(false)),
            Some(Literal::Bool(false))
        );
        assert_eq!(
            const_to_literal(&ConstValue::UInt(100)),
            Some(Literal::UInt(100))
        );
        // Compound types return None
        assert_eq!(const_to_literal(&ConstValue::Array(vec![])), None);
        assert_eq!(const_to_literal(&ConstValue::Map(vec![])), None);
    }

    #[test]
    fn test_const_index_array() {
        let arr = ConstValue::Array(vec![
            ConstValue::UInt(10),
            ConstValue::UInt(20),
            ConstValue::UInt(30),
        ]);

        assert_eq!(
            const_index(&arr, &ConstValue::UInt(0)),
            Some(ConstValue::UInt(10))
        );
        assert_eq!(
            const_index(&arr, &ConstValue::UInt(1)),
            Some(ConstValue::UInt(20))
        );
        assert_eq!(const_index(&arr, &ConstValue::UInt(3)), None); // Out of bounds
        assert_eq!(const_index(&arr, &ConstValue::Int(-1)), None); // Negative
    }

    #[test]
    fn test_const_index_text() {
        // Text indexing returns UInt code points (no Char type)
        let text = ConstValue::Text("hello".to_string());

        assert_eq!(
            const_index(&text, &ConstValue::UInt(0)),
            Some(ConstValue::UInt('h' as u64))
        );
        assert_eq!(
            const_index(&text, &ConstValue::UInt(4)),
            Some(ConstValue::UInt('o' as u64))
        );
        assert_eq!(const_index(&text, &ConstValue::UInt(5)), None); // Out of bounds
    }

    #[test]
    fn test_const_cast_identity() {
        // Identity casts: same type in, same type out
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::UInt(42), ConstValue::UInt(1)]
            ),
            Some(ConstValue::UInt(42))
        );
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Int(-5), ConstValue::UInt(2)]
            ),
            Some(ConstValue::Int(-5))
        );
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Float(3.14), ConstValue::UInt(3)]
            ),
            Some(ConstValue::Float(3.14))
        );
    }

    #[test]
    fn test_const_cast_bit_reinterpret() {
        // Int → UInt: bit reinterpret
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Int(-1), ConstValue::UInt(1)]
            ),
            Some(ConstValue::UInt(u64::MAX))
        );
        // UInt → Int: bit reinterpret
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::UInt(u64::MAX), ConstValue::UInt(2)]
            ),
            Some(ConstValue::Int(-1))
        );
    }

    #[test]
    fn test_const_cast_widen_to_float() {
        // UInt → Float
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::UInt(42), ConstValue::UInt(3)]
            ),
            Some(ConstValue::Float(42.0))
        );
        // Int → Float
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Int(-10), ConstValue::UInt(3)]
            ),
            Some(ConstValue::Float(-10.0))
        );
    }

    #[test]
    fn test_const_cast_invalid_source() {
        // Bool → UInt: not a valid cast source
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Bool(true), ConstValue::UInt(1)]
            ),
            None
        );
        // Text → Int: not a valid cast source
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Text("42".into()), ConstValue::UInt(2)]
            ),
            None
        );
        // Float → UInt: not supported (use floor/round/trunc)
        assert_eq!(
            eval_intrinsic_const(
                IntrinsicOp::Cast,
                &[ConstValue::Float(3.14), ConstValue::UInt(1)]
            ),
            None
        );
    }

    #[test]
    fn test_const_index_map() {
        let map = ConstValue::Map(vec![
            (ConstValue::Text("a".to_string()), ConstValue::UInt(1)),
            (ConstValue::Text("b".to_string()), ConstValue::UInt(2)),
        ]);

        assert_eq!(
            const_index(&map, &ConstValue::Text("a".to_string())),
            Some(ConstValue::UInt(1))
        );
        assert_eq!(
            const_index(&map, &ConstValue::Text("c".to_string())),
            None // Not found
        );
    }
}
