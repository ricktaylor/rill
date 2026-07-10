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
    // Initialize file-scope globals (a no-op for global-free programs).
    vm.exec(&program)
        .map_err(|e| format!("exec error: {}", e))?;
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

#[test]
fn test_let_no_initializer() {
    // `let x;` binds x to Undefined (SQL NULL semantics).
    let result = run("fn test() { let x; return x; }", "test").unwrap();
    assert!(result.is_undefined());
}

#[test]
fn test_let_no_initializer_then_assign() {
    // An uninitialized binding can be assigned later.
    let val = run_expect("fn test() { let x; x = 42; return x; }", "test");
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn test_let_no_initializer_propagates_undefined() {
    // Using an uninitialized variable propagates Undefined, not an error.
    let result = run("fn test() { let x; return x + 1; }", "test").unwrap();
    assert!(result.is_undefined());
}

// ========================================================================
// File-Scope Globals
// ========================================================================

/// Helper: assert a single-file program fails to compile.
fn compile_fails(source: &str) -> bool {
    let externs = externs::standard_externs();
    crate::compile(source, &externs).is_err()
}

#[test]
fn test_global_persists_across_calls() {
    // The global survives function returns: two inc() calls accumulate.
    let val = run_expect(
        r#"
        let count = 0;
        fn inc() { ::count = ::count + 1; }
        fn test() { inc(); inc(); ::count }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn test_global_const_like_read() {
    let val = run_expect(
        r#"
        let max = 100;
        fn test() { ::max }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(100));
}

#[test]
fn test_global_uninitialized_is_undefined() {
    let result = run(
        r#"
        let g;
        fn test() { ::g }
        "#,
        "test",
    )
    .unwrap();
    assert!(result.is_undefined());
}

#[test]
fn test_global_assigned_then_read() {
    let val = run_expect(
        r#"
        let g;
        fn test() { ::g = 7; ::g }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(7));
}

#[test]
fn test_global_compound_assignment() {
    let val = run_expect(
        r#"
        let n = 5;
        fn test() { ::n += 10; ::n }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(15));
}

#[test]
fn test_global_forward_reference_is_error() {
    // `a` is declared after `b`, so `b`'s initializer references it before it is
    // in scope — a use-before-definition error, exactly as in a function scope.
    assert!(compile_fails(
        r#"
        let b = a + 1;
        let a = 10;
        fn test() { ::b }
        "#
    ));
}

#[test]
fn test_global_self_reference_is_error() {
    // A global's initializer cannot reference itself (not yet in scope).
    assert!(compile_fails("let a = a + 1; fn test() { ::a }"));
}

#[test]
fn test_global_initializer_references_earlier_global() {
    // Forward order: `a` then `b = a + 1` → b is 11.
    let val = run_expect(
        r#"
        let a = 10;
        let b = a + 1;
        fn test() { ::b }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(11));
}

#[test]
fn test_bare_name_does_not_resolve_to_global() {
    // Inside a function, a bare name never resolves to a global — `::` required.
    assert!(compile_fails(
        r#"
        let count = 0;
        fn test() { count }
        "#
    ));
}

#[test]
fn test_global_clashes_with_function() {
    assert!(compile_fails(
        r#"
        let foo = 1;
        fn foo() { 2 }
        fn test() { 0 }
        "#
    ));
}

#[test]
fn test_global_clashes_with_intrinsic() {
    assert!(compile_fails("let len = 1;"));
}

#[test]
fn test_duplicate_global() {
    assert!(compile_fails("let x = 1; let x = 2;"));
}

#[test]
fn test_global_discard_rejected() {
    assert!(compile_fails("let _ = 5;"));
}

#[test]
fn test_clone_inherits_globals_independently() {
    // clone() is a deep copy: the child inherits initialized globals, then
    // mutations on each VM are independent.
    let externs = externs::standard_externs();
    let (program, _) = crate::compile(
        r#"
        let count = 0;
        fn inc() { ::count = ::count + 1; }
        fn get() { ::count }
        "#,
        &externs,
    )
    .expect("should compile");

    let mut vm = VM::new();
    vm.exec(&program).expect("exec");
    program.call(&mut vm, "inc", 0).expect("inc"); // parent: count = 1

    let mut worker = vm.clone();
    program.call(&mut worker, "inc", 0).expect("inc"); // worker: count = 2

    assert_eq!(program.call(&mut vm, "get", 0).unwrap(), Value::UInt(1));
    assert_eq!(program.call(&mut worker, "get", 0).unwrap(), Value::UInt(2));
}

#[test]
fn test_global_in_imported_file_unsupported() {
    // Multi-file globals are deferred — an imported file declaring a global
    // must produce a clear error, not silently misbehave.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("lib.rill", "let state = 0; fn get() { ::state }");
    loader.add_source(
        "main.rill",
        r#"
        import "lib.rill";
        fn test() { lib::get() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    assert!(compiler.build().is_err());
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
fn test_const_global_inlined() {
    // Former `const MAX = 100;` is now a file-scope global accessed via `::MAX`;
    // the optimizer inlines the never-written foldable global back to a constant.
    let val = run_expect(
        r#"
            let MAX = 100;
            fn test() { return ::MAX; }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(100));
}

#[test]
fn test_global_chained_initializer() {
    // A global whose initializer references an earlier global is computed once at
    // load time (it stays a runtime global, not inlined — but the value is right).
    let val = run_expect(
        r#"
            let MAX_TTL = 86400;
            let DOUBLE = MAX_TTL * 2;
            fn test() { ::DOUBLE }
            "#,
        "test",
    );
    assert_eq!(val, Value::UInt(172800));
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
    // A cast in a global initializer folds, and the global inlines to a constant.
    let val = run_expect(
        r#"
            let X = -1 as UInt;
            fn test() { ::X }
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
            ExternDef::new("math", "classify", generic)
                .param("x", TypeSet::numeric())
                .returns(TypeSet::uint())
                .pure_infallible()
                .variant(&[TypeSet::uint()], TypeSet::uint(), uint_variant)
                .variant(&[TypeSet::int()], TypeSet::uint(), int_variant),
        )
        .unwrap();

    // Compile with the custom registry
    let source = r#"
            require math as _;
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
    // Initialize globals before pushing args (exec resets the stack).
    vm.exec(&program).expect("exec failed");
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
    // 100,000 recursive calls — exceeds DEFAULT_STACK_SIZE (65536) without TCO
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
    // Uses `let` params — ack is a pure computation, no write-back needed.
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

// ========================================================================
// By-Ref Write-Back Tests
// ========================================================================

#[test]
fn byref_dump_accessor() {
    let source = r#"
        fn build_map(size) {
            let m = {};
            for i in 0..size {
                m[i] = i * i;
            }
            m
        }
    "#;
    let externs = externs::standard_externs();
    let mut diags = crate::diagnostics::Diagnostics::new();
    let ast = crate::ast::parser::parse(source, "", &mut diags).expect("parse failed");
    let ir = crate::ir::lower(&ast, &externs, &mut diags).expect("lower failed");
    eprintln!("=== BEFORE OPTIMIZATION ===");
    for func in &ir.functions {
        eprintln!("{}", func.dump());
    }
    let mut ir = ir;
    crate::opt::optimize(&mut ir, &externs, &mut diags);
    eprintln!("=== AFTER OPTIMIZATION ===");
    for func in &ir.functions {
        eprintln!("{}", func.dump());
    }
}

#[test]
fn byref_with_binding_writeback() {
    // with x = arr[0]; x = 42 → arr[0] should be 42
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2, 3];
            with x = arr[0];
            x = 42;
            arr[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn float_div_by_zero_undefined() {
    // Non-finite results are Undefined: 1.0 / 0.0 is not inf.
    // Runtime path (param defeats const folding).
    let val = run_expect(
        r#"
        fn f(a) {
            let d = 1.0 / a;
            match d {
                Float => { 1 }
                _ => { 2 }
            }
        }
        fn test() { f(0.0) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn float_div_by_zero_undefined_const() {
    // The fully-constant form agrees with the runtime: no fold to inf
    let val = run_expect(
        r#"
        fn test() {
            let d = 1.0 / 0.0;
            match d {
                Float => { 1 }
                _ => { 2 }
            }
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn float_overflow_undefined() {
    // Float overflow is Undefined, matching integer overflow semantics
    let val = run_expect(
        r#"
        fn f(a) {
            let b = a * a;
            let c = b * b;
            let d = c * c;
            match d {
                Float => { 1 }
                _ => { 2 }
            }
        }
        fn test() { f(10000000000000000000000000000000000000000.0) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn float_neg_zero_normalized() {
    // -x for x == 0.0 is +0.0: one zero, bitwise equality agrees with
    // numeric equality
    let val = run_expect(
        r#"
        fn f(x) {
            let nz = -x;
            if nz == 0.0 { 1 } else { 2 }
        }
        fn test() { f(0.0) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn float_neg_zero_single_map_key() {
    // A computed -0.0 and literal 0.0 are the SAME map key
    let val = run_expect(
        r#"
        fn f(x) {
            let m = {};
            m[-x] = 5;
            m[0.0]
        }
        fn test() { f(0.0) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn float_nonfinite_literal_undefined() {
    // A source literal beyond f64 range parses to inf → Undefined value
    let val = run_expect(
        r#"
        fn test() {
            let d = 1.0e999;
            match d {
                Float => { 1 }
                _ => { 2 }
            }
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn algebra_div_pow2_not_miscompiled() {
    // x / 4 must divide, not copy (the old strength-reduction stub
    // rewrote x / 2^k to x for any proven-UInt operand)
    let val = run_expect(
        r#"
        fn quarter(x) { return x / 4; }
        fn test() { return quarter(100); }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(25));
}

#[test]
fn algebra_mul_pow2_not_miscompiled() {
    // x * 4 must quadruple, not double (the old stub emitted x + x)
    let val = run_expect(
        r#"
        fn times4(x) { return x * 4; }
        fn test() { return times4(100); }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(400));
}

#[test]
fn algebra_float_sub_self_propagates_undefined() {
    // 1.0 / 0.0 is Undefined (checked float semantics), and x - x on a
    // possibly-undefined operand must not fold to zero
    let val = run_expect(
        r#"
        fn f(a) {
            let x = 1.0 / a;
            let d = x - x;
            match d {
                Int => { 2 }
                Float => { 3 }
                _ => { 1 }
            }
        }
        fn test() { f(0.0) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn algebra_self_eq_undefined_known_wrong() {
    // KNOWN BUG (pinned): x == x where x is Undefined (overflow) should
    // yield Undefined (branching to 2), but numeric_result_type omits
    // Undefined from refined arithmetic results, so type analysis
    // certifies x as defined and dead-arm elimination removes the
    // definedness guard as redundant. The algebra pass itself is gated
    // correctly (see test_self_eq_possibly_undefined_not_folded); the
    // fix belongs in the arithmetic type lattice.
    let val = run_expect(
        r#"
        fn test() {
            let x = 18446744073709551615 + 1;
            if x == x { 1 } else { 2 }
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn shift_amount_64_undefined() {
    // Shifts are checked like arithmetic: amount >= 64 → Undefined
    let val = run_expect(
        r#"
        fn f(s) {
            let d = 1 << s;
            match d {
                UInt => { 1 }
                _ => { 2 }
            }
        }
        fn test() { f(63) * 10 + f(64) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(12));
}

#[test]
fn shift_amount_64_undefined_const() {
    // The fully-constant form agrees with the runtime
    let val = run_expect(
        r#"
        fn test() {
            let d = 1 << 64;
            match d {
                UInt => { 1 }
                _ => { 2 }
            }
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn shift_right_64_undefined() {
    let val = run_expect(
        r#"
        fn f(s) {
            let d = 255 >> s;
            match d {
                UInt => { 1 }
                _ => { 2 }
            }
        }
        fn test() { f(4) * 10 + f(64) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(12));
}

#[test]
fn with_pattern_array_writeback() {
    // Pattern-bound element refs write back to the named scrutinee
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2];
            with [x, y] = arr;
            x = 9;
            y = 8;
            arr[0] + arr[1]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(17));
}

#[test]
fn with_pattern_map_writeback() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 1;
            with {a: v} = m;
            v = 5;
            m["a"]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn with_pattern_rest_edges_writeback() {
    // Elements on both sides of a rest pattern write back
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2, 3, 4];
            with [a, ..rest, z] = arr;
            a = 10;
            z = 40;
            arr[0] + arr[3]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(50));
}

#[test]
fn if_with_array_elem_writeback() {
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2];
            if with [x, y] = arr {
                x = 7;
            }
            arr[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(7));
}

#[test]
fn if_with_map_value_writeback() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["k"] = 1;
            if with {k: v} = m {
                v = 42;
            }
            m["k"]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn let_pattern_no_writeback() {
    // By-value pattern bindings copy; writes do not reach the scrutinee
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2];
            let [x, y] = arr;
            x = 9;
            arr[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn with_pattern_nested_scrutinee_no_writeback() {
    // A scrutinee that is itself an element access (`m["a"]`) does not
    // write back through the outer collection — nested accessor chains
    // are unsupported (the element ref would need a reloadable base name).
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = [1];
            with [x] = m["a"];
            x = 5;
            m["a"][0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn byref_call_arg_base_writeback() {
    // Passing an element ref to a by-ref param: the callee's write lands
    // in the base collection and is visible through the base name
    let val = run_expect(
        r#"
        fn set42(with p) { p = 42; }
        fn test() {
            let arr = [1, 2];
            with x = arr[0];
            set42(x);
            arr[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn byref_call_arg_alias_sees_callee_write() {
    // After the call, the element binding reads the callee's write — the
    // accessor survives optimization; it is not a bind-time snapshot.
    // (An alias does NOT track the base across LATER reload generations:
    // each write-back reload migrates the live value to a fresh SSA def,
    // and slot-resident accessors keep aliasing the old one. See TODO.md.)
    let val = run_expect(
        r#"
        fn set42(with p) { p = 42; }
        fn test() {
            let arr = [1, 2];
            with x = arr[0];
            set42(x);
            x
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(42));
}

#[test]
fn byref_alias_sees_element_write() {
    // A read-only `with` alias over a written base must observe the write
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2, 3];
            with x = arr[0];
            arr[0] = 99;
            x
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(99));
}

#[test]
fn byref_pattern_alias_no_stale_cse() {
    // A write through a pattern-bound element ref must not let CSE serve
    // the pre-write value for a later read of the same element
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2];
            with [x] = arr;
            let a = arr[0];
            x = 99;
            let b = arr[0];
            a + b
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(100));
}

#[test]
fn byref_array_int_key_writeback() {
    // Int (non-negative) index keys are accepted by writes, symmetric with reads
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2, 3];
            let i = 0 as Int;
            arr[i] = 99;
            arr[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(99));
}

#[test]
fn byref_let_binding_no_writeback() {
    // let x = arr[0]; x = 42 → arr[0] unchanged
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2, 3];
            let x = arr[0];
            x = 42;
            arr[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn byref_default_param_is_byval() {
    // Default params are by-value — callee can't modify caller's variable
    let val = run_expect(
        r#"
        fn modify(x) {
            x = 99;
        }
        fn test() {
            let a = 5;
            modify(a);
            a
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn byref_with_param_writeback() {
    // with-param: callee mutation visible to caller
    let val = run_expect(
        r#"
        fn set_to_zero(with x) {
            x = 0;
        }
        fn test() {
            let a = 42;
            set_to_zero(a);
            a
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0));
}

#[test]
fn byref_array_mutation_through_param() {
    // with-param: callee mutates array element, caller sees it
    let val = run_expect(
        r#"
        fn set_first(with arr) {
            arr[0] = 99;
        }
        fn test() {
            let a = [1, 2, 3];
            set_first(a);
            a[0]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(99));
}

#[test]
fn byref_recursive_countdown() {
    // Recursive by-ref: count_down(with c) decrements through Ref chain
    let val = run_expect(
        r#"
        fn count_down(with c) {
            if c > 0 {
                c = c - 1;
                count_down(c);
            }
        }
        fn test() {
            let x = 5;
            count_down(x);
            x
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0));
}

#[test]
fn byref_no_writeback_without_with() {
    // Same function without `with` — by-value, no write-back
    let val = run_expect(
        r#"
        fn count_down(c) {
            if c > 0 {
                c = c - 1;
                count_down(c);
            }
        }
        fn test() {
            let x = 5;
            count_down(x);
            x
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

// ========================================================================
// Module System: Compiler Builder + MemoryLoader
// ========================================================================

#[test]
fn test_compiler_single_file() {
    // Single file via Compiler builder (no imports)
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("main.rill", "fn test() { 42 }");

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(42));
}

#[test]
fn test_compiler_multi_file_import() {
    // Two files: main imports utils
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("utils.rill", "fn helper() { 99 }");
    loader.add_source(
        "main.rill",
        r#"
        import "utils.rill";
        fn test() { utils::helper() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(99));
}

#[test]
fn test_compiler_import_with_alias() {
    // Import with alias: `import "utils.rill" as helpers`
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("utils.rill", "fn helper() { 42 }");
    loader.add_source(
        "main.rill",
        r#"
        import "utils.rill" as helpers;
        fn test() { helpers::helper() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(42));
}

#[test]
fn test_compiler_transitive_imports() {
    // Three files: main → utils → common
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("common.rill", "fn base_val() { 10 }");
    loader.add_source(
        "utils.rill",
        r#"
        import "common.rill";
        fn helper() { common::base_val() + 1 }
        "#,
    );
    loader.add_source(
        "main.rill",
        r#"
        import "utils.rill";
        fn test() { utils::helper() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(11));
}

#[test]
fn test_compiler_diamond_imports() {
    // Diamond: main imports A and B, both import common
    // common should be loaded only once
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("common.rill", "fn shared() { 7 }");
    loader.add_source(
        "a.rill",
        r#"
        import "common.rill";
        fn from_a() { common::shared() + 1 }
        "#,
    );
    loader.add_source(
        "b.rill",
        r#"
        import "common.rill";
        fn from_b() { common::shared() + 2 }
        "#,
    );
    loader.add_source(
        "main.rill",
        r#"
        import "a.rill";
        import "b.rill";
        fn test() { a::from_a() + b::from_b() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    // from_a = 7+1 = 8, from_b = 7+2 = 9, total = 17
    assert_eq!(result, Value::UInt(17));
}

#[test]
fn test_compiler_import_not_found() {
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source(
        "main.rill",
        r#"
        import "nonexistent.rill";
        fn test() { 1 }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let result = compiler.build();
    // Should fail with an error about the missing import
    assert!(result.is_err());
}

#[test]
fn test_compiler_duplicate_namespace() {
    // Two imports with the same default namespace
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add("a/utils.rill", "utils", "fn from_a() { 1 }");
    loader.add("b/utils.rill", "utils", "fn from_b() { 2 }");
    loader.add_source(
        "main.rill",
        r#"
        import "a/utils.rill";
        import "b/utils.rill";
        fn test() { 1 }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let result = compiler.build();
    // Should fail: duplicate namespace "utils"
    assert!(result.is_err());
}

#[test]
fn test_compiler_add_source_direct() {
    // Using add_source() for single-file compilation (no loader)
    let loader = crate::loader::MemoryLoader::new();
    let mut compiler = crate::Compiler::new(&loader);
    compiler.add_source("fn test() { 123 }", "inline");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(123));
}

#[test]
fn test_compiler_import_as_underscore() {
    // import "utils.rill" as _ — merge into root scope
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("utils.rill", "fn helper() { 77 }");
    loader.add_source(
        "main.rill",
        r#"
        import "utils.rill" as _;
        fn test() { helper() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(77));
}

#[test]
fn test_compiler_import_as_underscore_multiple_functions() {
    // as _ with multiple functions from imported file
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source(
        "math.rill",
        r#"
        fn add_one(x) { x + 1 }
        fn double(x) { x * 2 }
        "#,
    );
    loader.add_source(
        "main.rill",
        r#"
        import "math.rill" as _;
        fn test() { double(add_one(5)) }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("build should succeed");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    // add_one(5) = 6, double(6) = 12
    assert_eq!(result, Value::UInt(12));
}

// ============================================================================
// Module Phase 4 — visibility + dead-import elimination
// ============================================================================

/// Collect the unused-import (W010) warnings from a built program's diagnostics.
fn w010_warnings(
    diags: &crate::diagnostics::Diagnostics,
) -> Vec<&crate::diagnostics::Diagnostic> {
    diags
        .warnings()
        .filter(|d| d.code == crate::diagnostics::DiagnosticCode::W010_UnusedImport)
        .collect()
}

#[test]
fn test_dce_dead_import_pruned_and_warns() {
    // main imports `dead.rill` but never calls anything from it.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("dead.rill", "fn unused() { 1 }");
    let main_src = "import \"dead.rill\";\nfn test() { 5 }\n";
    loader.add_source("main.rill", main_src);

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    // The dead imported function is gone from the compiled program.
    assert!(!program.compiled.func_index.contains_key("dead::unused"));

    // Exactly one W010, attributed to the root file, pointing at the import.
    let warnings = w010_warnings(&diags);
    assert_eq!(warnings.len(), 1);
    let w = warnings[0];
    assert_eq!(w.source_id.as_deref(), Some("main.rill"));
    // The span starts at the `import` keyword and covers through the `;`
    // (the parser's trailing `padded_by(whitespace())` may extend it further).
    let span = w.span.expect("W010 should carry a span");
    let import_start = main_src.find("import").unwrap();
    let import_end = main_src.find(';').unwrap();
    assert_eq!(span.start, import_start);
    assert!(span.end >= import_end + 1);

    // The program still runs.
    let mut vm = VM::new();
    assert_eq!(program.call(&mut vm, "test", 0).unwrap(), Value::UInt(5));
}

#[test]
fn test_dce_kept_import_works() {
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("utils.rill", "fn helper() { 42 }");
    loader.add_source(
        "main.rill",
        r#"
        import "utils.rill";
        fn test() { utils::helper() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(program.compiled.func_index.contains_key("utils::helper"));
    assert!(w010_warnings(&diags).is_empty());

    let mut vm = VM::new();
    assert_eq!(program.call(&mut vm, "test", 0).unwrap(), Value::UInt(42));
}

#[test]
fn test_dce_partial_pruning_no_warning() {
    // One imported function is used, another isn't: prune the dead one, but the
    // import is still "used" so no warning.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source(
        "lib.rill",
        r#"
        fn live() { 1 }
        fn dead() { 2 }
        "#,
    );
    loader.add_source(
        "main.rill",
        r#"
        import "lib.rill";
        fn test() { lib::live() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(program.compiled.func_index.contains_key("lib::live"));
    assert!(!program.compiled.func_index.contains_key("lib::dead"));
    assert!(w010_warnings(&diags).is_empty());
}

#[test]
fn test_dce_as_underscore_unused_warns() {
    // `as _` import whose merged function is never called.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("extra.rill", "fn thing() { 1 }");
    loader.add_source(
        "main.rill",
        r#"
        import "extra.rill" as _;
        fn test() { 9 }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(!program.compiled.func_index.contains_key("extra::thing"));
    assert_eq!(w010_warnings(&diags).len(), 1);
}

#[test]
fn test_dce_as_underscore_used_no_warning() {
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("extra.rill", "fn thing() { 8 }");
    loader.add_source(
        "main.rill",
        r#"
        import "extra.rill" as _;
        fn test() { thing() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(program.compiled.func_index.contains_key("extra::thing"));
    assert!(w010_warnings(&diags).is_empty());
    let mut vm = VM::new();
    assert_eq!(program.call(&mut vm, "test", 0).unwrap(), Value::UInt(8));
}

#[test]
fn test_dce_diamond_dead_leg() {
    // main imports a and b; both import common; main uses only a.
    // b::from_b is dead and pruned; common::shared stays live via a::from_a.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("common.rill", "fn shared() { 7 }");
    loader.add_source(
        "a.rill",
        r#"
        import "common.rill";
        fn from_a() { common::shared() + 1 }
        "#,
    );
    loader.add_source(
        "b.rill",
        r#"
        import "common.rill";
        fn from_b() { common::shared() + 2 }
        "#,
    );
    loader.add_source(
        "main.rill",
        r#"
        import "a.rill";
        import "b.rill";
        fn test() { a::from_a() }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(program.compiled.func_index.contains_key("a::from_a"));
    assert!(program.compiled.func_index.contains_key("common::shared"));
    assert!(!program.compiled.func_index.contains_key("b::from_b"));

    // Only the `b` import is unused.
    let warnings = w010_warnings(&diags);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].message.contains("b.rill"));

    let mut vm = VM::new();
    assert_eq!(program.call(&mut vm, "test", 0).unwrap(), Value::UInt(8));
}

#[test]
fn test_dce_init_only_reachability() {
    // An imported function reachable only from a root global initializer (i.e.
    // only via `__init__`) must survive — `__init__` is a root.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("helper.rill", "fn compute() { 100 }");
    loader.add_source(
        "main.rill",
        r#"
        import "helper.rill";
        let g = helper::compute();
        fn test() { ::g }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(program.compiled.func_index.contains_key("helper::compute"));
    assert!(w010_warnings(&diags).is_empty());
}

#[test]
fn test_dce_dead_import_cycle() {
    // Mutually-recursive imported functions, neither reachable from root.
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source(
        "cyc.rill",
        r#"
        fn x() { cyc::y() }
        fn y() { cyc::x() }
        "#,
    );
    loader.add_source(
        "main.rill",
        r#"
        import "cyc.rill";
        fn test() { 3 }
        "#,
    );

    let mut compiler = crate::Compiler::new(&loader);
    compiler.add("main.rill");
    let (program, diags) = compiler.build().expect("build should succeed");

    assert!(!program.compiled.func_index.contains_key("cyc::x"));
    assert!(!program.compiled.func_index.contains_key("cyc::y"));
    assert_eq!(w010_warnings(&diags).len(), 1);
}

#[test]
fn test_dce_uncalled_root_function_kept() {
    // A root-file function nothing calls is a potential embedder entry point and
    // must NOT be pruned (DCE keys on file origin, never on in-degree).
    let loader = crate::loader::MemoryLoader::new();
    let mut compiler = crate::Compiler::new(&loader);
    compiler.add_source(
        r#"
        fn entry_a() { 1 }
        fn entry_b() { 2 }
        "#,
        "main.rill",
    );
    let (program, _diags) = compiler.build().expect("build should succeed");

    assert!(program.compiled.func_index.contains_key("entry_a"));
    assert!(program.compiled.func_index.contains_key("entry_b"));
    let mut vm = VM::new();
    assert_eq!(program.call(&mut vm, "entry_b", 0).unwrap(), Value::UInt(2));
}

// ============================================================================
// Unused-variable lint (W001)
// ============================================================================

/// Collect the unused-variable (W001) warnings from a built program's diagnostics.
fn w001_warnings(
    diags: &crate::diagnostics::Diagnostics,
) -> Vec<&crate::diagnostics::Diagnostic> {
    diags
        .warnings()
        .filter(|d| d.code == crate::diagnostics::DiagnosticCode::W001_UnusedVariable)
        .collect()
}

fn build_single(src: &str) -> (crate::Program, crate::diagnostics::Diagnostics) {
    let loader = crate::loader::MemoryLoader::new();
    let mut compiler = crate::Compiler::new(&loader);
    compiler.add_source(src, "main.rill");
    compiler.build().expect("build should succeed")
}

#[test]
fn test_w001_unused_let_warns() {
    let src = "fn test() { let x = 5; 0 }";
    let (_program, diags) = build_single(src);

    let warnings = w001_warnings(&diags);
    assert_eq!(warnings.len(), 1);
    let w = warnings[0];
    assert!(w.message.contains("unused variable `x`"));
    assert_eq!(w.source_id.as_deref(), Some("main.rill"));
    let span = w.span.expect("W001 should carry a span");
    assert_eq!(span.start, src.find('x').unwrap());
}

#[test]
fn test_w001_used_let_no_warning() {
    let (_program, diags) = build_single("fn test() { let x = 5; x }");
    assert!(w001_warnings(&diags).is_empty());
}

#[test]
fn test_w001_param_not_warned() {
    // Parameters are contracts; a never-read param is not flagged.
    let (_program, diags) = build_single("fn test(p) { 0 }");
    assert!(w001_warnings(&diags).is_empty());
}

#[test]
fn test_w001_discard_not_warned() {
    // `_` creates no binding.
    let (_program, diags) = build_single("fn test() { let _ = 5; 0 }");
    assert!(w001_warnings(&diags).is_empty());
}

#[test]
fn test_w001_with_binding_not_warned() {
    // `with` bindings mutate their base via WriteRef — a never-read `with` is a
    // side effect, not an unused variable.
    let (_program, diags) = build_single("fn test() { let a = [10, 20]; with x = a[0]; x = 1; 0 }");
    let ws = w001_warnings(&diags);
    assert!(ws.is_empty(), "unexpected W001: {:?}", ws.iter().map(|w| &w.message).collect::<Vec<_>>());
}

#[test]
fn test_w001_shadowing_warns_first_only() {
    // The first `x` is shadowed before any read → unused; the second is read.
    let src = "fn test() { let x = 1; let x = 2; x }";
    let (_program, diags) = build_single(src);
    let warnings = w001_warnings(&diags);
    assert_eq!(warnings.len(), 1);
    // The flagged decl is the first `x`.
    let span = warnings[0].span.expect("span");
    assert_eq!(span.start, src.find('x').unwrap());
}

// ============================================================================
// Slot allocator
// ============================================================================

#[test]
fn slot_alloc_accessor_survives_intervening_temps() {
    // A `with` accessor into arr[1], then several temps that — without pinning —
    // could be coalesced onto the accessor's base/key/ref slots, then a write
    // through the accessor. Pinning keeps the accessor's captured slots valid,
    // so the write lands in arr[1]. This would corrupt if pinning were dropped.
    let val = run_expect(
        r#"
        fn test() {
            let arr = [10, 20, 30];
            with x = arr[1];
            let t1 = 1;
            let t2 = t1 + 10;
            let t3 = t2 + 20;
            let t4 = t3 + 5;
            x = t1 + t2 + t3 + t4;
            arr[1]
        }
        "#,
        "test",
    );
    // t1=1, t2=11, t3=31, t4=36 → 79
    assert_eq!(val, Value::UInt(79));
}

#[test]
fn slot_alloc_shrinks_frame_for_disjoint_temps() {
    // A chain of disjoint temporaries (each dies as the next is computed) packs
    // into far fewer slots than the number of SSA variables. The param keeps the
    // chain out of the constant folder.
    let (program, _diags) = build_single(
        r#"
        fn test(n) {
            let a = n + 1;
            let b = a + 1;
            let c = b + 1;
            let d = c + 1;
            let e = d + 1;
            let f = e + 1;
            return f;
        }
        "#,
    );
    let fs = program.function_frame_size("test").expect("frame size");
    // ~38 SSA vars (each guarded `+` expands to const/guard/copy/add/phi) pack
    // into a single-digit frame — coalescing is clearly working.
    assert!(fs <= 12, "expected a tight frame, got {fs}");

    let mut vm = VM::new();
    vm.push(Value::UInt(0)).unwrap();
    assert_eq!(program.call(&mut vm, "test", 1).unwrap(), Value::UInt(6));
}

// ============================================================================
// Map content iteration (for k, v in map)
// ============================================================================

#[test]
fn test_map_iter_real_keys() {
    // Documented example: the key binds to the actual map key, not a counter.
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["priority"] = 5;
            m["other"] = 1;
            for key, value in m {
                if key == "priority" { return value; }
            }
            return 0;
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(5));
}

#[test]
fn test_map_iter_sum_values() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 10;
            m["b"] = 20;
            m["c"] = 30;
            let sum = 0;
            for k, v in m { sum = sum + v; }
            return sum;
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(60));
}

#[test]
fn test_map_iter_single_binding_is_value() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 3;
            m["b"] = 4;
            let sum = 0;
            for x in m { sum = sum + x; }
            return sum;
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(7));
}

#[test]
fn test_map_iter_byref_writeback_string_keys() {
    // `with` value binding writes back to map[key] for string-keyed maps.
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 1;
            m["b"] = 2;
            for with k, v in m { v = v * 10; }
            return m["a"] + m["b"];
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(30));
}

#[test]
fn test_map_iter_byval_no_writeback() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 1;
            for let k, v in m { v = 99; }
            return m["a"];
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(1));
}

#[test]
fn test_map_iter_empty() {
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            let count = 0;
            for k, v in m { count = count + 1; }
            return count;
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(0));
}

#[test]
fn test_map_iter_numeric_key_writeback() {
    // UInt-keyed maps write back like any other: VM::set dispatches on the
    // base's type, and the loop accessor aliases the named map.
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m[0] = 1;
            for with k, v in m { v = 99; }
            return m[0];
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(99));
}

#[test]
fn for_with_array_writeback() {
    // By-ref loop over a locally-typed array writes back
    let val = run_expect(
        r#"
        fn test() {
            let arr = [1, 2, 3];
            for with x in arr { x = x * 2; }
            arr[0] + arr[2]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(8));
}

#[test]
fn for_with_array_writeback_through_param() {
    // The iterable's static type is any() here (function param) — the
    // dispatcher's narrowing copy must not swallow the write-back.
    let val = run_expect(
        r#"
        fn double(a) {
            for with x in a { x = x * 2; }
            a[0]
        }
        fn test() { double(collect(1..4)) }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn for_with_array_pair_writeback() {
    // Pair binding: index stays by-value, value writes back
    let val = run_expect(
        r#"
        fn test() {
            let arr = [10, 20];
            for with i, v in arr { v = v + i; }
            arr[0] + arr[1]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(31));
}

#[test]
fn for_with_map_key_stays_byval() {
    // Assigning the key binding must not touch the map's keys
    let val = run_expect(
        r#"
        fn test() {
            let m = {};
            m["a"] = 1;
            for with k, v in m { k = "z"; }
            m["a"] + len(m)
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(2));
}

#[test]
fn for_with_monomorphized_array_and_map() {
    // The same by-ref loop body works for both iterable types after
    // monomorphization clones the function per call signature.
    let val = run_expect(
        r#"
        fn scaled_at(c, k) {
            for with v in c { v = v * 10; }
            c[k]
        }
        fn test() {
            let arr = [1];
            let m = {};
            m["k"] = 2;
            scaled_at(arr, 0) + scaled_at(m, "k")
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(30));
}

#[test]
fn byref_param_iterable_loop_writeback() {
    // A by-ref param iterated by-ref: writes reach the CALLER's array on
    // every iteration (the accessor hangs off the param's stable ref var).
    let val = run_expect(
        r#"
        fn dbl(with a) {
            for with x in a { x = x * 2; }
        }
        fn test() {
            let arr = [1, 2];
            dbl(arr);
            arr[0] + arr[1]
        }
        "#,
        "test",
    );
    assert_eq!(val, Value::UInt(6));
}

#[test]
fn test_compiler_import_require_namespace_clash() {
    // import and require both claim the same namespace — should error
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("math.rill", "fn add(x, y) { x + y }");
    loader.add_source(
        "main.rill",
        r#"
        require math;
        import "math.rill";
        fn test() { 1 }
        "#,
    );

    let externs = crate::standard_externs();
    let mut compiler = crate::Compiler::with_externs(externs, &loader);
    // Register a "math" extern namespace so the require is valid
    compiler
        .add_extern(crate::ExternDef::new("math", "sin", |_vm, _argc| {
            Ok(crate::externs::ExecResult::Return(Value::Float(
                crate::exec::Float::new(0.0).unwrap(),
            )))
        }))
        .unwrap();
    compiler.add("main.rill");
    let result = compiler.build();
    assert!(
        result.is_err(),
        "should error on import/require namespace clash"
    );
}

#[test]
fn test_compiler_import_require_no_clash_with_alias() {
    // import with alias avoids the clash
    let mut loader = crate::loader::MemoryLoader::new();
    loader.add_source("math.rill", "fn add(x, y) { x + y }");
    loader.add_source(
        "main.rill",
        r#"
        require math;
        import "math.rill" as rill_math;
        fn test() { rill_math::add(1, 2) }
        "#,
    );

    let externs = crate::standard_externs();
    let mut compiler = crate::Compiler::with_externs(externs, &loader);
    compiler
        .add_extern(crate::ExternDef::new("math", "sin", |_vm, _argc| {
            Ok(crate::externs::ExecResult::Return(Value::Float(
                crate::exec::Float::new(0.0).unwrap(),
            )))
        }))
        .unwrap();
    compiler.add("main.rill");
    let (program, _warnings) = compiler.build().expect("should succeed with alias");

    let mut vm = VM::new();
    let result = program
        .call(&mut vm, "test", 0)
        .expect("call should succeed");
    assert_eq!(result, Value::UInt(3));
}

#[test]
fn test_pretty_error_rendering() {
    // Verify that compilation errors include source context.
    // Use a parse error (precise span) rather than a lowering error.
    let source = "fn test( { }";
    let externs = crate::standard_externs();
    let err = match crate::compile(source, &externs) {
        Err(e) => e,
        Ok(_) => panic!("expected compilation error"),
    };
    let rendered = format!("{}", err);

    // Should have file:line:col location
    assert!(
        rendered.contains("--> <input>:"),
        "should show source file: {}",
        rendered
    );
    // Should show the source line
    assert!(
        rendered.contains("fn test("),
        "should show source line: {}",
        rendered
    );
    // Should have caret underline
    assert!(
        rendered.contains("^"),
        "should have caret underline: {}",
        rendered
    );
}

// ========================================================================
// IR Guard Coverage — visual inspection tests
// ========================================================================

/// Helper: parse, lower, optimize, and dump the IR for a named function.
/// Prints the IR to stderr (visible with `cargo test -- --nocapture`).
fn dump_ir(source: &str, func_name: &str) -> String {
    let externs = externs::standard_externs();
    let mut diagnostics = crate::diagnostics::Diagnostics::new();
    let source_id: std::rc::Rc<str> = std::rc::Rc::from("<test>");
    diagnostics
        .source_map
        .add(source_id.clone(), source.to_string());
    diagnostics.set_source(source_id);

    let ast = crate::ast::parser::parse(source, "<test>", &mut diagnostics)
        .expect("parse should succeed");
    let mut ir_program =
        crate::ir::lower(&ast, &externs, &mut diagnostics).expect("lowering should succeed");
    crate::opt::optimize(&mut ir_program, &externs, &mut diagnostics);

    if diagnostics.has_warnings() {
        eprintln!("--- Diagnostics ---\n{}", diagnostics);
    }

    let func = ir_program
        .functions
        .iter()
        .find(|f| f.name.as_ref() == func_name)
        .unwrap_or_else(|| panic!("function '{}' not found in IR", func_name));

    let ir = func.dump();
    eprintln!("--- IR for {} ---\n{}", func_name, ir);
    ir
}

#[test]
fn inspect_for_loop_len_guards() {
    // For-loop over an array — verify the index path has:
    // 1. A narrowing Copy excluding Sequence
    // 2. A type guard Match before Len
    let ir = dump_ir(
        "fn sum(arr) {
            let total = 0;
            for x in arr {
                total = total + x;
            }
            total
        }",
        "sum",
    );

    // The Len intrinsic should appear in the IR
    assert!(ir.contains("Len"), "IR should contain Len intrinsic");

    // Verify it executes correctly
    let val = run_expect(
        "fn sum(arr) {
            let total = 0;
            for x in arr {
                total = total + x;
            }
            total
        }
        fn test() { sum([1, 2, 3, 4, 5]) }",
        "test",
    );
    assert_eq!(val, Value::UInt(15));
}

#[test]
fn inspect_range_guards() {
    // Range expression — verify type guards are emitted for
    // user-supplied operands (Add, Lt, MakeSeq).
    let ir = dump_ir(
        "fn count(n) {
            let total = 0;
            for i in 0..n {
                total = total + 1;
            }
            total
        }",
        "count",
    );

    // The MakeSeq intrinsic should appear in the IR
    assert!(
        ir.contains("MakeSeq"),
        "IR should contain MakeSeq intrinsic"
    );

    // Verify it executes correctly
    let val = run_expect(
        "fn count(n) {
            let total = 0;
            for i in 0..n {
                total = total + 1;
            }
            total
        }
        fn test() { count(10) }",
        "test",
    );
    assert_eq!(val, Value::UInt(10));
}

#[test]
fn inspect_inclusive_range_guards() {
    // Inclusive range — verify end+1 Add guard works
    let ir = dump_ir(
        "fn count(n) {
            let total = 0;
            for i in 0..=n {
                total = total + 1;
            }
            total
        }",
        "count",
    );

    assert!(
        ir.contains("MakeSeq"),
        "IR should contain MakeSeq intrinsic"
    );

    let val = run_expect(
        "fn count(n) {
            let total = 0;
            for i in 0..=n {
                total = total + 1;
            }
            total
        }
        fn test() { count(9) }",
        "test",
    );
    assert_eq!(val, Value::UInt(10));
}

#[test]
fn inspect_collect_range() {
    dump_ir(
        r#"
            fn test() {
                let arr = collect(0..5);
                return len(arr);
            }
        "#,
        "test",
    );
}

#[test]
fn inspect_benchmarks() {
    let source = include_str!("../../docs/benchmarks.rill");
    let funcs = [
        "fib",
        "fib_iter",
        "make_tree",
        "check_tree",
        "binary_trees",
        "ack",
        "tak",
        "is_prime",
        "sum_primes",
        "collatz_length",
        "max_collatz",
        "build_map",
        "sum_map_values",
        "map_benchmark",
        "array_sum",
        "matrix_trace",
        "popcount",
        "hamming_distance",
        "bitwise_benchmark",
    ];
    for name in funcs {
        dump_ir(source, name);
    }
}
