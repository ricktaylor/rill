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

/// Evaluate a builtin by name with constant arguments
///
/// This is a convenience wrapper for when you have a string name
/// rather than a FunctionRef.
pub fn eval_builtin_const_by_name(
    name: &str,
    args: &[ConstValue],
    registry: &BuiltinRegistry,
) -> Option<ConstValue> {
    let builtin = registry.get(name)?;
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
        // Text[UInt] -> single character as Text
        (ConstValue::Text(s), ConstValue::UInt(idx)) => s
            .chars()
            .nth(*idx as usize)
            .map(|c| ConstValue::Text(c.to_string())),
        // Text[Int] (if non-negative)
        (ConstValue::Text(s), ConstValue::Int(idx)) => {
            if *idx >= 0 {
                s.chars()
                    .nth(*idx as usize)
                    .map(|c| ConstValue::Text(c.to_string()))
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
// Short-Circuit Logic
// ============================================================================

/// Evaluate logical AND with short-circuit semantics
///
/// - If first arg is `false`, returns `false` without evaluating second
/// - If both are known booleans, returns the result
/// - Otherwise returns `None`
pub fn const_and(args: &[ConstValue]) -> Option<ConstValue> {
    if args.is_empty() {
        return None;
    }

    // Short-circuit: false && _ = false
    if let ConstValue::Bool(false) = &args[0] {
        return Some(ConstValue::Bool(false));
    }

    // Both args needed for full evaluation
    if args.len() >= 2
        && let (ConstValue::Bool(a), ConstValue::Bool(b)) = (&args[0], &args[1])
    {
        return Some(ConstValue::Bool(*a && *b));
    }

    None
}

/// Evaluate logical OR with short-circuit semantics
///
/// - If first arg is `true`, returns `true` without evaluating second
/// - If both are known booleans, returns the result
/// - Otherwise returns `None`
pub fn const_or(args: &[ConstValue]) -> Option<ConstValue> {
    if args.is_empty() {
        return None;
    }

    // Short-circuit: true || _ = true
    if let ConstValue::Bool(true) = &args[0] {
        return Some(ConstValue::Bool(true));
    }

    // Both args needed for full evaluation
    if args.len() >= 2
        && let (ConstValue::Bool(a), ConstValue::Bool(b)) = (&args[0], &args[1])
    {
        return Some(ConstValue::Bool(*a || *b));
    }

    None
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
        let text = ConstValue::Text("hello".to_string());

        assert_eq!(
            const_index(&text, &ConstValue::UInt(0)),
            Some(ConstValue::Text("h".to_string()))
        );
        assert_eq!(
            const_index(&text, &ConstValue::UInt(4)),
            Some(ConstValue::Text("o".to_string()))
        );
        assert_eq!(const_index(&text, &ConstValue::UInt(5)), None); // Out of bounds
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

    #[test]
    fn test_const_and() {
        // Short-circuit
        assert_eq!(
            const_and(&[ConstValue::Bool(false)]),
            Some(ConstValue::Bool(false))
        );

        // Full evaluation
        assert_eq!(
            const_and(&[ConstValue::Bool(true), ConstValue::Bool(true)]),
            Some(ConstValue::Bool(true))
        );
        assert_eq!(
            const_and(&[ConstValue::Bool(true), ConstValue::Bool(false)]),
            Some(ConstValue::Bool(false))
        );
    }

    #[test]
    fn test_const_or() {
        // Short-circuit
        assert_eq!(
            const_or(&[ConstValue::Bool(true)]),
            Some(ConstValue::Bool(true))
        );

        // Full evaluation
        assert_eq!(
            const_or(&[ConstValue::Bool(false), ConstValue::Bool(false)]),
            Some(ConstValue::Bool(false))
        );
        assert_eq!(
            const_or(&[ConstValue::Bool(false), ConstValue::Bool(true)]),
            Some(ConstValue::Bool(true))
        );
    }
}
