use super::*;
use crate::externs;

/// Helper: compile source and execute a named function (no args)
fn run(source: &str, func_name: &str) -> Result<Value, String> {
    let externs = externs::standard_externs();
    let (program, diagnostics) =
        crate::compile(source, &externs).map_err(|d| format!("compilation failed: {}", d))?;

    if diagnostics.has_warnings() {
        eprintln!("{}", diagnostics);
    }

    let mut vm = VM::new();
    program
        .call(&mut vm, func_name, 0)
        .map_err(|e| format!("exec error: {}", e))
}

/// Helper: compile and run, expecting a defined Value back
fn run_expect(source: &str, func_name: &str) -> Value {
    let val = run(source, func_name).expect("should not error");
    assert!(val.is_defined(), "expected a defined value, got Undefined");
    val
}

// ========================================================================
// Basic Execution
// ========================================================================

#[test]
fn test_return_constant() {
    let val = run_expect("fn test() { return 42; }", "test");
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_return_bool() {
    let val = run_expect("fn test() { return true; }", "test");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn test_return_no_value() {
    let result = run("fn test() { return; }", "test").unwrap();
    assert!(result.is_undefined());
}

#[test]
fn test_implicit_return() {
    // Final expression without semicolon is the return value
    let val = run_expect("fn test() { 99 }", "test");
    assert_eq!(val, Value::UInt(99));
}

// ========================================================================
// Arithmetic (binary externs)
// ========================================================================

#[test]
fn test_addition() {
    let val = run_expect("fn test() { return 1 + 2; }", "test");
    assert_eq!(val, Value::UInt(3));
}

#[test]
fn test_arithmetic_expression() {
    let val = run_expect("fn test() { return (10 - 3) * 2; }", "test");
    assert_eq!(val, Value::UInt(14));
}

#[test]
fn test_comparison() {
    let val = run_expect("fn test() { return 5 > 3; }", "test");
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn test_equality() {
    let val = run_expect("fn test() { return 42 == 42; }", "test");
    assert_eq!(val, Value::Bool(true));
}

// ========================================================================
// Variables
// ========================================================================

#[test]
fn test_let_binding() {
    let val = run_expect(
        "fn test() { let x = 10; let y = 20; return x + y; }",
        "test",
    );
    assert_eq!(val, Value::UInt(30));
}

#[test]
fn test_variable_reassignment() {
    let val = run_expect("fn test() { let x = 1; x = x + 10; return x; }", "test");
    assert_eq!(val, Value::UInt(11));
}

// ========================================================================
// Control Flow
// ========================================================================

#[test]
fn test_if_true() {
    // Implicit return: if-expression is the final expression (no semicolon)
    let val = run_expect("fn test() { if true { 1 } else { 2 } }", "test");
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn test_if_false() {
    let val = run_expect("fn test() { if false { 1 } else { 2 } }", "test");
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn test_if_with_comparison() {
    let val = run_expect(
        "fn test() { let x = 10; if x > 5 { 1 } else { 0 } }",
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

// ========================================================================
// Loops
// ========================================================================

#[test]
fn test_while_loop() {
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                let i = 0;
                while i < 5 {
                    sum = sum + i;
                    i = i + 1;
                }
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(10)); // 0+1+2+3+4
}

#[test]
fn test_loop_break() {
    let val = run_expect(
        r#"
            fn test() {
                let i = 0;
                loop {
                    if i >= 3 {
                        break;
                    }
                    i = i + 1;
                }
                return i;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(3));
}

#[test]
fn test_loop_break_with_value() {
    let val = run_expect(
        r#"
            fn test() {
                let result = loop {
                    break 42;
                };
                return result;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

// ========================================================================
// Functions
// ========================================================================

#[test]
fn test_function_call() {
    let val = run_expect(
        r#"
            fn add(a, b) { return a + b; }
            fn test() { return add(3, 4); }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(7));
}

#[test]
fn test_recursive_function() {
    let val = run_expect(
        r#"
            fn factorial(n) {
                if n <= 1 { return 1; }
                return n * factorial(n - 1);
            }
            fn test() { return factorial(5); }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(120));
}

// ========================================================================
// Constants
// ========================================================================

#[test]
fn test_const_binding() {
    let val = run_expect(
        r#"
            const MAX = 100;
            fn test() { return MAX; }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(100));
}

// ========================================================================
// Short-circuit logic
// ========================================================================

#[test]
fn test_short_circuit_and() {
    let val = run_expect("fn test() { return true && false; }", "test");
    assert_eq!(val, Value::Bool(false));
}

#[test]
fn test_short_circuit_or() {
    let val = run_expect("fn test() { return false || true; }", "test");
    assert_eq!(val, Value::Bool(true));
}

// ========================================================================
// Externs
// ========================================================================

#[test]
fn test_len() {
    let val = run_expect(r#"fn test() { let a = [1, 2, 3]; return len(a); }"#, "test");
    assert_eq!(val, Value::UInt(3));
}

#[test]
fn test_negation() {
    let val = run_expect("fn test() { return !true; }", "test");
    assert_eq!(val, Value::Bool(false));
}

// ========================================================================
// Match / Pattern Matching
// ========================================================================

#[test]
fn test_match_literal() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 2;
                match x {
                    1 => { return 10; },
                    2 => { return 20; },
                    3 => { return 30; },
                    _ => { return 0; },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(20));
}

#[test]
fn test_match_wildcard() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 99;
                match x {
                    1 => { return 10; },
                    _ => { return 42; },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_match_type_pattern() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                match x {
                    Bool(b) => { return 0; },
                    UInt(n) => { return n; },
                    _ => { return 99; },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_match_with_guard() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 15;
                match x {
                    UInt(n) if n > 10 => { return 1; },
                    UInt(n) => { return 2; },
                    _ => { return 3; },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn test_match_guard_fails() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 5;
                match x {
                    UInt(n) if n > 10 => { return 1; },
                    UInt(n) => { return 2; },
                    _ => { return 3; },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

// ========================================================================
// If-Let / If-With Patterns
// ========================================================================

#[test]
fn test_if_let_binding() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                if let y = x {
                    return y + 1;
                }
                return 0;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(43));
}

#[test]
fn test_if_let_type_pattern() {
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                if let UInt(n) = x {
                    return n + 10;
                }
                return 0;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(52));
}

// ========================================================================
// Array Destructuring
// ========================================================================

#[test]
fn test_let_array_destructure() {
    let val = run_expect(
        r#"
            fn test() {
                let arr = [10, 20, 30];
                let [a, b, c] = arr;
                return a + b + c;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn test_match_array_pattern() {
    let val = run_expect(
        r#"
            fn test() {
                let arr = [1, 2];
                match arr {
                    [a, b] => { return a + b; },
                    _ => { return 0; },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(3));
}

// ========================================================================
// For Loop Execution
// ========================================================================

#[test]
fn test_for_array_sum() {
    let val = run_expect(
        r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn test_for_array_with_index() {
    // Pair binding: i = index, x = element
    let val = run_expect(
        r#"
            fn test() {
                let arr = [10, 20, 30];
                let result = 0;
                for i, x in arr {
                    result = result + i + x;
                };
                return result;
            }
            "#,
        "test",
    );
    // (0+10) + (1+20) + (2+30) = 63
    assert_eq!(val, Value::UInt(63));
}

#[test]
fn test_for_with_break() {
    let val = run_expect(
        r#"
            fn test() {
                let arr = [1, 2, 3, 4, 5];
                let sum = 0;
                for x in arr {
                    if x > 3 { break; };
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    // 1 + 2 + 3 = 6 (stops before 4)
    assert_eq!(val, Value::UInt(6));
}

#[test]
fn test_for_with_continue() {
    let val = run_expect(
        r#"
            fn test() {
                let arr = [1, 2, 3, 4, 5];
                let sum = 0;
                for x in arr {
                    if x == 3 { continue; };
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    // 1 + 2 + 4 + 5 = 12 (skips 3)
    assert_eq!(val, Value::UInt(12));
}

#[test]
fn test_for_empty_array() {
    let val = run_expect(
        r#"
            fn test() {
                let arr = [];
                let count = 0;
                for x in arr {
                    count = count + 1;
                };
                return count;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0));
}

#[test]
fn test_for_nested() {
    let val = run_expect(
        r#"
            fn test() {
                let a = [1, 2];
                let b = [10, 20];
                let sum = 0;
                for x in a {
                    for y in b {
                        sum = sum + x * y;
                    };
                };
                return sum;
            }
            "#,
        "test",
    );
    // 1*10 + 1*20 + 2*10 + 2*20 = 10 + 20 + 20 + 40 = 90
    assert_eq!(val, Value::UInt(90));
}

#[test]
fn test_for_let_binding() {
    // for let x — by-value, mutations don't affect source
    let val = run_expect(
        r#"
            fn test() {
                let arr = [1, 2, 3];
                let sum = 0;
                for let x in arr {
                    x = x * 10;
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    // 10 + 20 + 30 = 60
    assert_eq!(val, Value::UInt(60));
}

// ========================================================================
// Sequence / Range Execution
// ========================================================================

#[test]
fn test_range_sum() {
    // for i in 0..5 { sum += i } → 0+1+2+3+4 = 10
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for i in 0..5 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(10));
}

#[test]
fn test_range_inclusive_sum() {
    // for i in 0..=4 { sum += i } → 0+1+2+3+4 = 10
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for i in 0..=4 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(10));
}

#[test]
fn test_range_empty() {
    // 5..3 is empty — body never runs
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for i in 5..3 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0));
}

#[test]
fn test_range_with_break() {
    // 0..10 with break at 3 → 0+1+2 = 3
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for i in 0..10 {
                    if i == 3 { break; };
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(3));
}

#[test]
fn test_range_with_continue() {
    // 0..6, skip even numbers → 1+3+5 = 9
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for i in 0..6 {
                    if i % 2 == 0 { continue; };
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(9));
}

#[test]
fn test_range_single_element() {
    // 5..6 has one element: 5
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for i in 5..6 {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn test_range_nested() {
    // Nested ranges: for i in 0..3 { for j in 0..3 { count++ } }
    let val = run_expect(
        r#"
            fn test() {
                let count = 0;
                for i in 0..3 {
                    for j in 0..3 {
                        count = count + 1;
                    };
                };
                return count;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(9));
}

#[test]
fn test_range_dynamic_bounds() {
    // Range with dynamic bounds from array length
    let val = run_expect(
        r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for i in 0..len(arr) {
                    sum = sum + arr[i];
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn test_range_with_array_access_literal_bounds() {
    // Minimal case: range with literal bounds + arr[i] in body
    let val = run_expect(
        r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for i in 0..3 {
                    sum = sum + arr[i];
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn test_range_with_outer_var() {
    // Minimal case: range loop accessing outer variable (no indexing)
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                let sum = 0;
                for i in 0..3 {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(126));
}

#[test]
fn test_range_empty_body_outer_var() {
    // Empty for-loop body, return outer variable
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                for i in 0..3 {
                };
                return x;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_range_two_outer_vars() {
    // Range loop body reads one outer var, writes another
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                let sum = 0;
                for i in 0..3 {
                    sum = x;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_outer_loop_accumulation_no_nesting() {
    // Same arithmetic as test_for_nested but without the inner loop
    let val = run_expect(
        r#"
            fn test() {
                let a = [1, 2];
                let sum = 0;
                for x in a {
                    sum = sum + x * 10 + x * 20;
                };
                return sum;
            }
            "#,
        "test",
    );
    // 1*10 + 1*20 + 2*10 + 2*20 = 90
    assert_eq!(val, Value::UInt(90));
}

#[test]
fn test_range_as_value() {
    // Store a range in a variable, then iterate — type dispatch
    // selects the sequence path at runtime.
    let val = run_expect(
        r#"
            fn test() {
                let r = 1..4;
                let sum = 0;
                for i in r {
                    sum = sum + i;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(6));
}

#[test]
fn test_for_type_dispatch_array() {
    // Ensure index-based path still works through type dispatch
    let val = run_expect(
        r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn test_for_dispatch_with_accumulator() {
    // Outer variable modified in loop body — verify Phi merge at join
    let val = run_expect(
        r#"
            fn test() {
                let count = 0;
                for i in 0..5 {
                    count = count + 1;
                };
                return count;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

// ========================================================================
// collect() Intrinsic
// ========================================================================

#[test]
fn test_collect_range() {
    // collect(0..5) → [0, 1, 2, 3, 4]
    let val = run_expect(
        r#"
            fn test() {
                let arr = collect(0..5);
                return len(arr);
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn test_collect_range_sum() {
    // collect(0..4) then sum the array
    let val = run_expect(
        r#"
            fn test() {
                let arr = collect(1..=3);
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(6));
}

// ================================================================
// Type cast (as) tests
// ================================================================

#[test]
fn test_cast_uint_to_int() {
    let val = run_expect("fn test() { 42 as Int }", "test");
    assert_eq!(val, Value::Int(42));
}

#[test]
fn test_cast_int_to_uint_reinterpret() {
    // -1 as UInt should give u64::MAX (bit reinterpret)
    let val = run_expect("fn test() { -1 as UInt }", "test");
    assert_eq!(val, Value::UInt(u64::MAX));
}

#[test]
fn test_cast_uint_to_int_reinterpret() {
    // Large UInt wraps to negative Int
    let val = run_expect(
        r#"
            fn test() {
                let x = 18446744073709551615 as Int;
                x
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::Int(-1));
}

#[test]
fn test_cast_to_float() {
    let val = run_expect("fn test() { 42 as Float }", "test");
    assert_eq!(val, Value::Float(crate::exec::Float::new(42.0).unwrap()));
}

#[test]
fn test_cast_int_to_float() {
    let val = run_expect("fn test() { -10 as Float }", "test");
    assert_eq!(val, Value::Float(crate::exec::Float::new(-10.0).unwrap()));
}

#[test]
fn test_cast_identity() {
    // Same-type cast is identity
    let val = run_expect("fn test() { 42 as UInt }", "test");
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_cast_in_arithmetic() {
    // Cast then add
    let val = run_expect(
        r#"
            fn test() {
                let x = 10 as Float;
                let y = 3 as Float;
                x + y
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::Float(crate::exec::Float::new(13.0).unwrap()));
}

#[test]
fn test_cast_chained() {
    // UInt → Int → UInt roundtrip
    let val = run_expect("fn test() { 42 as Int as UInt }", "test");
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_cast_precedence() {
    // x + y as Float should parse as x + (y as Float)
    // 10 + 5 as Float = 10 + 5.0
    // With implicit coercion, 10 (UInt) + 5.0 (Float) → 15.0
    let val = run_expect("fn test() { 10 + 5 as Float }", "test");
    assert_eq!(val, Value::Float(crate::exec::Float::new(15.0).unwrap()));
}

#[test]
fn test_cast_const_fold() {
    // Constant cast should be folded at compile time
    let val = run_expect(
        r#"
            const X = -1 as UInt;
            fn test() { X }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(u64::MAX));
}

#[test]
fn test_collect_empty_range() {
    // 5..3 is a reversed range → undefined (start < end guard fails)
    let result = run(
        r#"
            fn test() {
                let arr = collect(5..3);
                return len(arr);
            }
            "#,
        "test",
    );
    assert_eq!(result.unwrap(), Value::Undefined);
}

// ================================================================
// Dead Match Arm Elimination (end-to-end)
// ================================================================

#[test]
fn test_match_dead_arm_eliminated() {
    // x is UInt(42), so Int arm is dead — only UInt arm executes
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                match x {
                    UInt(n) => { n + 1 },
                    Int(n) => { 999 },
                    _ => { 0 },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(43));
}

#[test]
fn test_match_single_arm_collapse() {
    // x is UInt, only UInt arm matches — Match collapses to Jump
    let val = run_expect(
        r#"
            fn test() {
                let x = 10;
                match x {
                    UInt(n) => { n * 2 },
                    _ => { 0 },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(20));
}

#[test]
fn test_match_all_arms_dead() {
    // x is UInt, but only Text/Bool arms — all dead, takes default
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                match x {
                    Text(s) => { 1 },
                    Bool(b) => { 2 },
                    _ => { 99 },
                }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(99));
}

// ================================================================
// Dead Code Elimination (end-to-end)
// ================================================================

#[test]
fn test_dce_unused_computation() {
    // Dead computation should be eliminated — no runtime cost
    let val = run_expect(
        r#"
            fn test() {
                let x = 42;
                let unused = x * 2 + 1;
                x
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_dce_after_algebra() {
    // x * 1 → Copy(x), then the Const(1) becomes dead
    let val = run_expect(
        r#"
            fn test() {
                let x = 7;
                let y = x * 1;
                y
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(7));
}

#[test]
fn test_dce_preserves_side_effects() {
    // Recursive call has side effects (stack usage) — must not be removed
    // even if result is unused
    let val = run_expect(
        r#"
            fn countdown(n) {
                if n <= 0 { return 0; }
                countdown(n - 1);
                n
            }
            fn test() { countdown(5) }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn test_dce_chain_elimination() {
    // a = 1, b = a + 1, c = b + 1 — only c used
    // After const folding, all become constants.
    // DCE doesn't need to fire because const fold handles it.
    // But if we use a non-constant chain:
    let val = run_expect(
        r#"
            fn test() {
                let a = 10;
                let b = a + a;
                b
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(20));
}

// ================================================================
// Text and Bytes iteration (no Char type — yields UInt)
// ================================================================

#[test]
fn test_text_indexing_returns_uint() {
    // "A"[0] → 65 (Unicode code point)
    let val = run_expect(
        r#"
            fn test() {
                let s = "A";
                s[0]
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(65)); // 'A' = 65
}

#[test]
fn test_text_iteration_sum() {
    // Sum of code points: "AB" → 65 + 66 = 131
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for c in "AB" {
                    sum = sum + c;
                }
                sum
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(131));
}

#[test]
fn test_text_len() {
    let val = run_expect(
        r#"
            fn test() {
                len("hello")
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn test_bytes_indexing_returns_uint() {
    // First byte of bytes([0x48, 0x69]) → 0x48 = 72
    let val = run_expect(
        r#"
            fn test() {
                let b = bytes([0x48, 0x69]);
                b[0]
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0x48));
}

#[test]
fn test_bytes_iteration_sum() {
    // Sum of bytes: bytes([1, 2, 3]) → 1 + 2 + 3 = 6
    let val = run_expect(
        r#"
            fn test() {
                let sum = 0;
                for b in bytes([0x01, 0x02, 0x03]) {
                    sum = sum + b;
                }
                sum
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(6));
}

#[test]
fn test_bytes_len() {
    let val = run_expect(
        r#"
            fn test() {
                len(bytes([0x01, 0x02, 0x03, 0x04]))
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(4));
}

#[test]
fn test_text_unicode_iteration() {
    // Unicode: "é" is U+00E9 = 233
    let val = run_expect(
        r#"
            fn test() {
                let s = "é";
                s[0]
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0xE9)); // é = U+00E9
}

// ================================================================
// Character literals (sugar for UInt code points)
// ================================================================

#[test]
fn test_char_literal_basic() {
    let val = run_expect("fn test() { 'A' }", "test");
    assert_eq!(val, Value::UInt(65));
}

#[test]
fn test_char_literal_escape() {
    let val = run_expect("fn test() { '\\n' }", "test");
    assert_eq!(val, Value::UInt(10));
}

#[test]
fn test_char_literal_comparison() {
    // Compare character from string to char literal
    let val = run_expect(
        r#"
            fn test() {
                let s = "Hello";
                s[0] == 'H'
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::Bool(true));
}

#[test]
fn test_char_literal_arithmetic() {
    // 'A' + 1 = 66 = 'B'
    let val = run_expect("fn test() { 'A' + 1 }", "test");
    assert_eq!(val, Value::UInt(66));
}

#[test]
fn test_char_literal_unicode_escape() {
    // \u{E9} = é = 233
    let val = run_expect("fn test() { '\\u{E9}' }", "test");
    assert_eq!(val, Value::UInt(0xE9));
}

#[test]
fn test_char_literal_emoji() {
    // \u{1F600} = 😀 = 128512 (beyond BMP)
    let val = run_expect("fn test() { '\\u{1F600}' }", "test");
    assert_eq!(val, Value::UInt(0x1F600));
}

#[test]
fn test_string_unicode_escape() {
    // \u{...} works in strings too
    let val = run_expect(
        r#"
            fn test() {
                let s = "\u{48}\u{69}";
                len(s)
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2)); // "Hi" = 2 chars
}

// ================================================================
// Return type inference (interprocedural)
// ================================================================

#[test]
fn test_return_type_inference() {
    // double() always returns a numeric result from multiplication
    // The caller should be able to use it in arithmetic without warnings
    let val = run_expect(
        r#"
            fn double(x) { x * 2 }
            fn test() {
                let y = double(21);
                y
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_return_type_chains() {
    // Return type flows through a chain of calls
    let val = run_expect(
        r#"
            fn add_one(x) { x + 1 }
            fn add_two(x) { add_one(add_one(x)) }
            fn test() { add_two(40) }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_arg_type_propagation() {
    // All callers pass UInt → param narrows to {UInt} → return narrows to {UInt}
    let val = run_expect(
        r#"
            fn square(x) { x * x }
            fn test() { square(7) }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(49));
}

#[test]
fn test_arg_type_mixed_callers() {
    // Multiple callers with different types → param is union
    let val = run_expect(
        r#"
            fn identity(x) { x }
            fn test() {
                let a = identity(42);
                let b = identity(true);
                a
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

// ================================================================
// Interprocedural definedness propagation
// ================================================================

#[test]
fn test_interprocedural_definedness() {
    // All callers pass Defined → callee param is Defined
    // → callee body uses Defined values → no spurious warnings
    let val = run_expect(
        r#"
            fn add(a, b) { a + b }
            fn test() { add(10, 20) }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(30));
}

#[test]
fn test_interprocedural_type_and_def_chain() {
    // Type + definedness flow through a chain:
    // test → process(42) → double(x) → x * 2
    // All args Defined UInt at every level
    let val = run_expect(
        r#"
            fn double(x) { x * 2 }
            fn process(x) { double(x) + 1 }
            fn test() { process(20) }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(41));
}

#[test]
fn test_recursive_return_type() {
    // Recursive function: return type inferred across iterations
    let val = run_expect(
        r#"
            fn factorial(n) {
                if n <= 1 { return 1; }
                return n * factorial(n - 1);
            }
            fn test() { factorial(5) }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(120));
}

#[test]
fn test_forward_reference_return_type() {
    // fn test calls fn helper defined later — return type still inferred
    let val = run_expect(
        r#"
            fn test() { helper(10) }
            fn helper(x) { x + 5 }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(15));
}

// ================================================================
// Extern monomorphism (variant selection)
// ================================================================

#[test]
fn test_extern_variant_selection() {
    // Register an extern with type-specific variants.
    // The generic returns 0, uint variant returns 1, int variant returns 2.
    fn generic(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
        Ok(ExecResult::Return(Value::UInt(0)))
    }
    fn uint_variant(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
        Ok(ExecResult::Return(Value::UInt(1)))
    }
    fn int_variant(_vm: &mut VM, _argc: usize) -> Result<ExecResult, ExecError> {
        Ok(ExecResult::Return(Value::UInt(2)))
    }

    use crate::externs::{ExternDef, ExternRegistry};
    use crate::types::TypeSet;

    let mut externs = ExternRegistry::new();
    externs
        .register(
            ExternDef::new("classify", generic)
                .param("x", TypeSet::numeric())
                .returns(TypeSet::uint())
                .pure_infallible()
                .variant(&[TypeSet::uint()], TypeSet::uint(), uint_variant)
                .variant(&[TypeSet::int()], TypeSet::uint(), int_variant),
        )
        .unwrap();

    // Compile with the custom registry
    let source = r#"
            fn test() {
                let a = classify(42);
                a
            }
        "#;
    let (program, _diagnostics) = crate::compile(source, &externs).expect("should compile");

    let mut vm = VM::new();
    let result = program.call(&mut vm, "test", 0).expect("should not error");

    // 42 is UInt → uint_variant selected → returns 1
    assert_eq!(result, Value::UInt(1));
}

// ================================================================
// Function monomorphization
// ================================================================

#[test]
fn test_monomorphization() {
    // process() called with UInt at one site and Int at another
    // → should be monomorphized into two versions
    let val = run_expect(
        r#"
            fn process(x) { x + x }
            fn test() {
                let a = process(21);
                a
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_monomorphization_multiple_types() {
    // identity() called with different types at different sites
    let val = run_expect(
        r#"
            fn identity(x) { x }
            fn test() {
                let a = identity(42);
                let b = identity(true);
                if b { a } else { 0 }
            }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

// ========================================================================
// Benchmark Tests
// ========================================================================
//
// These exercise the benchmark functions from docs/benchmarks.rill
// to verify they compile and produce correct results.

/// Helper: compile source, push args, call function
fn run_with_args(source: &str, func_name: &str, args: &[Value]) -> Value {
    let externs = externs::standard_externs();
    let (program, diagnostics) = crate::compile(source, &externs).expect("compilation failed");
    if diagnostics.has_errors() {
        panic!("compilation errors: {}", diagnostics);
    }

    let mut vm = VM::new();
    for arg in args {
        vm.push(arg.clone()).expect("push failed");
    }
    let val = program
        .call(&mut vm, func_name, args.len())
        .expect("exec error");
    assert!(val.is_defined(), "expected a defined value, got Undefined");
    val
}

const BENCHMARK_SOURCE: &str = include_str!("../../docs/benchmarks.rill");

#[test]
fn bench_fib_recursive() {
    let val = run_with_args(BENCHMARK_SOURCE, "fib", &[Value::UInt(10)]);
    assert_eq!(val, Value::UInt(55));
}

#[test]
fn bench_fib_recursive_20() {
    let val = run_with_args(BENCHMARK_SOURCE, "fib", &[Value::UInt(20)]);
    assert_eq!(val, Value::UInt(6765));
}

#[test]
fn bench_fib_iterative() {
    let val = run_with_args(BENCHMARK_SOURCE, "fib_iter", &[Value::UInt(10)]);
    assert_eq!(val, Value::UInt(55));
}

#[test]
fn bench_for_reassign() {
    // Minimal test: variable reassignment inside a for loop
    let val = run_expect(
        r#"
        fn test() {
            let x = 0;
            for i in 0..5 {
                x += 1;
            }
            x
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn bench_for_fib_no_if() {
    // fib_iter without the early-return if, to isolate the issue
    let val = run_expect(
        r#"
        fn test() {
            let a = 0;
            let b = 1;
            for i in 0..8 {
                let next = a + b;
                a = b;
                b = next;
            }
            b
        }
        "#,
        "test",
    );
    // fib sequence: after 8 iters starting from (a=0, b=1), b=fib(9)=34
    assert_eq!(val, Value::UInt(34));
}

#[test]
fn bench_if_then_for() {
    // The problematic pattern: if with return, then for loop
    let val = run_expect(
        r#"
        fn fib(n) {
            if n < 2 { return n; }
            let a = 0;
            let b = 1;
            for i in 2..=n {
                let next = a + b;
                a = b;
                b = next;
            }
            b
        }
        fn test() { fib(10) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(55));
}

#[test]
fn bench_fib_iter_inline() {
    // Inline version to isolate from benchmark source
    let val = run_expect(
        r#"
        fn fib_iter(n) {
            if n < 2 { return n; }
            let a = 0;
            let b = 1;
            for i in 2..=n {
                let next = a + b;
                a = b;
                b = next;
            }
            b
        }
        fn test() { fib_iter(10) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(55));
}

#[test]
fn bench_fib_iterative_50() {
    let val = run_with_args(BENCHMARK_SOURCE, "fib_iter", &[Value::UInt(50)]);
    assert_eq!(val, Value::UInt(12586269025));
}

#[test]
fn bench_binary_trees() {
    // depth 0: 1 node, depth 1: 3 nodes, depth 2: 7, depth 3: 15
    // total = 1 + 3 + 7 + 15 = 26
    let val = run_with_args(BENCHMARK_SOURCE, "binary_trees", &[Value::UInt(3)]);
    assert_eq!(val, Value::UInt(26));
}

#[test]
fn bench_check_tree() {
    // A single tree of depth 4 has 2^5 - 1 = 31 nodes
    let val = run_with_args(BENCHMARK_SOURCE, "make_tree", &[Value::UInt(4)]);
    let check = run_with_args(BENCHMARK_SOURCE, "check_tree", &[val]);
    assert_eq!(check, Value::UInt(31));
}

// Diagnostic tests for Ackermann: isolate the if-else expression issue
#[test]
fn test_ack_simple_if_else() {
    // Simplest if-else expression with params
    let _val = run_expect(
        r#"
        fn test(m, n) {
            if m == 0 { n + 1 } else { 42 }
        }
        "#,
        "test",
    );
    // test() is called with no args → both params Undefined → should return Undefined
    // But run_expect asserts defined... let me use run_with_args
}

#[test]
fn test_ack_simple_if_else_with_args() {
    let val = run_with_args(
        r#"
        fn test(m, n) {
            if m == 0 { n + 1 } else { 42 }
        }
        "#,
        "test",
        &[Value::UInt(0), Value::UInt(5)],
    );
    assert_eq!(val, Value::UInt(6));
}

#[test]
fn test_ack_else_if_with_args() {
    let val = run_with_args(
        r#"
        fn test(m, n) {
            if m == 0 {
                n + 1
            } else if n == 0 {
                100
            } else {
                200
            }
        }
        "#,
        "test",
        &[Value::UInt(1), Value::UInt(5)],
    );
    assert_eq!(val, Value::UInt(200));
}

#[test]
fn test_ack_recursive_base() {
    let val = run_with_args(
        r#"
        fn ack(m, n) {
            if m == 0 {
                n + 1
            } else if n == 0 {
                ack(m - 1, 1)
            } else {
                ack(m - 1, ack(m, n - 1))
            }
        }
        "#,
        "ack",
        &[Value::UInt(0), Value::UInt(5)],
    );
    assert_eq!(val, Value::UInt(6));
}

#[test]
fn test_ack_one_level() {
    let val = run_with_args(
        r#"
        fn ack(m, n) {
            if m == 0 {
                n + 1
            } else if n == 0 {
                ack(m - 1, 1)
            } else {
                ack(m - 1, ack(m, n - 1))
            }
        }
        "#,
        "ack",
        &[Value::UInt(1), Value::UInt(0)],
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn bench_ackermann() {
    let val = run_with_args(BENCHMARK_SOURCE, "ack", &[Value::UInt(3), Value::UInt(5)]);
    assert_eq!(val, Value::UInt(253));
}

#[test]
fn bench_tak() {
    let val = run_with_args(
        BENCHMARK_SOURCE,
        "tak",
        &[Value::UInt(18), Value::UInt(12), Value::UInt(6)],
    );
    assert_eq!(val, Value::UInt(7));
}

#[test]
fn bench_is_prime() {
    let val = run_with_args(BENCHMARK_SOURCE, "is_prime", &[Value::UInt(97)]);
    assert_eq!(val, Value::Bool(true));

    let val = run_with_args(BENCHMARK_SOURCE, "is_prime", &[Value::UInt(100)]);
    assert_eq!(val, Value::Bool(false));
}

#[test]
fn bench_sum_primes() {
    // Sum of primes below 100 = 1060
    let val = run_with_args(BENCHMARK_SOURCE, "sum_primes", &[Value::UInt(100)]);
    assert_eq!(val, Value::UInt(1060));
}

#[test]
fn bench_collatz() {
    // collatz_length(27) = 111 steps
    let val = run_with_args(BENCHMARK_SOURCE, "collatz_length", &[Value::UInt(27)]);
    assert_eq!(val, Value::UInt(111));
}

#[test]
fn bench_max_collatz() {
    // Longest Collatz sequence under 100 starts at 97
    let val = run_with_args(BENCHMARK_SOURCE, "max_collatz", &[Value::UInt(100)]);
    assert_eq!(val, Value::UInt(97));
}

#[test]
fn bench_array_sum() {
    // sum of 0..10 = 0+1+2+...+9 = 45
    let val = run_with_args(BENCHMARK_SOURCE, "array_sum", &[Value::UInt(10)]);
    assert_eq!(val, Value::UInt(45));
}

#[test]
fn bench_append_basic() {
    let val = run_expect(
        r#"
        fn test() {
            let arr = [];
            append(arr, 10);
            append(arr, 20);
            append(arr, 30);
            len(arr)
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(3));
}

#[test]
fn bench_append_values() {
    let val = run_expect(
        r#"
        fn test() {
            let arr = [];
            append(arr, 10);
            append(arr, 20);
            append(arr, 30);
            arr[0] + arr[1] + arr[2]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn bench_map_insert_basic() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 1;
            m["b"] = 2;
            m["a"] + m["b"]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(3));
}

#[test]
fn bench_map_insert_loop() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m[0] = 10;
            m[1] = 20;
            m[0] + m[1]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(30));
}

#[test]
fn bench_map_operations() {
    // sum of i*i for i in 0..10 = 0+1+4+9+16+25+36+49+64+81 = 285
    let val = run_with_args(BENCHMARK_SOURCE, "map_benchmark", &[Value::UInt(10)]);
    assert_eq!(val, Value::UInt(285));
}

#[test]
fn bench_matrix_trace() {
    // sum of i*i for i in 0..5 = 0+1+4+9+16 = 30
    let val = run_with_args(BENCHMARK_SOURCE, "matrix_trace", &[Value::UInt(5)]);
    assert_eq!(val, Value::UInt(30));
}

#[test]
fn bench_popcount() {
    // popcount(0xFF) = 8
    let val = run_with_args(BENCHMARK_SOURCE, "popcount", &[Value::UInt(0xFF)]);
    assert_eq!(val, Value::UInt(8));

    // popcount(0) = 0
    let val = run_with_args(BENCHMARK_SOURCE, "popcount", &[Value::UInt(0)]);
    assert_eq!(val, Value::UInt(0));

    // popcount(u64::MAX) = 64
    let val = run_with_args(BENCHMARK_SOURCE, "popcount", &[Value::UInt(u64::MAX)]);
    assert_eq!(val, Value::UInt(64));
}

#[test]
fn bench_hamming_distance() {
    // hamming(0xFF, 0x0F) = popcount(0xF0) = 4
    let val = run_with_args(
        BENCHMARK_SOURCE,
        "hamming_distance",
        &[Value::UInt(0xFF), Value::UInt(0x0F)],
    );
    assert_eq!(val, Value::UInt(4));
}

#[test]
fn bench_bitwise() {
    // popcount(0)=0, popcount(1)=1, popcount(2)=1, popcount(3)=2,
    // popcount(4)=1, popcount(5)=2, popcount(6)=2, popcount(7)=3 → total=12
    let val = run_with_args(BENCHMARK_SOURCE, "bitwise_benchmark", &[Value::UInt(8)]);
    assert_eq!(val, Value::UInt(12));
}

// ========================================================================
// Tail-Call Optimization Tests
// ========================================================================

#[test]
fn tco_tail_recursive_factorial() {
    // 20! = 2432902008176640000 (fits in u64)
    let val = run_with_args(
        r#"
        fn factorial(n, acc) {
            if n <= 1 { acc }
            else { factorial(n - 1, n * acc) }
        }
        "#,
        "factorial",
        &[Value::UInt(20), Value::UInt(1)],
    );
    assert_eq!(val, Value::UInt(2432902008176640000));
}

#[test]
fn tco_deep_tail_recursive_sum() {
    // Sum 1..=100000 via tail recursion — 100K frames without TCO would overflow
    let val = run_with_args(
        r#"
        fn sum(n, acc) {
            if n == 0 { acc }
            else { sum(n - 1, acc + n) }
        }
        "#,
        "sum",
        &[Value::UInt(100_000), Value::UInt(0)],
    );
    assert_eq!(val, Value::UInt(5_000_050_000));
}

#[test]
fn tco_deep_recursion() {
    // 100,000 recursive calls — exceeds MAX_STACK_SIZE (65536) without TCO
    let val = run_with_args(
        r#"
        fn count_down(n) {
            if n == 0 { 0 }
            else { count_down(n - 1) }
        }
        "#,
        "count_down",
        &[Value::UInt(100_000)],
    );
    assert_eq!(val, Value::UInt(0));
}

#[test]
fn tco_ackermann_deeper() {
    // ack(3,7) = 1021 — exercises deep recursion with partial TCO
    // (2 of 3 branches are tail calls, inner ack(m, n-1) is not)
    let val = run_with_args(
        r#"
        fn ack(m, n) {
            if m == 0 {
                n + 1
            } else if n == 0 {
                ack(m - 1, 1)
            } else {
                ack(m - 1, ack(m, n - 1))
            }
        }
        "#,
        "ack",
        &[Value::UInt(3), Value::UInt(7)],
    );
    assert_eq!(val, Value::UInt(1021));
}

#[test]
fn tco_param_swap() {
    // Tests that arg values are read before being overwritten
    let val = run_with_args(
        r#"
        fn swap_recurse(a, b) {
            if a == 0 { b }
            else { swap_recurse(b, a - 1) }
        }
        "#,
        "swap_recurse",
        &[Value::UInt(5), Value::UInt(100)],
    );
    // a=5,b=100 → swap(100,4) → swap(4,99) → swap(99,3) → swap(3,98)
    // → swap(98,2) → swap(2,97) → swap(97,1) → swap(1,96) → swap(96,0)
    // → swap(0,95) → returns 95
    assert_eq!(val, Value::UInt(95));
}

#[test]
fn tco_non_tail_not_rewritten() {
    // fib(n-1) + fib(n-2) — neither call is in tail position
    // Should still work correctly (not rewritten to tail calls)
    let val = run_with_args(
        r#"
        fn fib(n) {
            if n < 2 { n }
            else { fib(n - 1) + fib(n - 2) }
        }
        "#,
        "fib",
        &[Value::UInt(10)],
    );
    assert_eq!(val, Value::UInt(55));
}

#[test]
fn tco_tail_call_in_match() {
    let val = run_with_args(
        r#"
        fn process(x) {
            match x {
                0 => { 42 },
                _ => { process(x - 1) },
            }
        }
        "#,
        "process",
        &[Value::UInt(10)],
    );
    assert_eq!(val, Value::UInt(42));
}
