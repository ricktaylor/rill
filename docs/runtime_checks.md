# Runtime Checks and Type Guards

## Current Architecture

Undefined is a type (`BaseType::Undefined`). All intrinsics require defined,
correctly-typed inputs. Undefined poisons everything — like SQL NULL or NaN.

### Three-tier type checking

1. **Types proven at compile time**: `eliminate_dead_match_arms` removes the
   guard entirely. `try_specialize_binary` emits direct `u64::checked_add`.
   Zero dispatch at runtime.

2. **Types unknown but guarded**: expression-level type guards emit Match
   before each intrinsic arg. The peephole layer (future) fuses `Match + Op`
   into a single dispatch step — no redundant type checking.

3. **Types unknown, no guard**: the intrinsic closure's internal `match`
   handles type dispatch at runtime. This is the fallback when guards
   are not present (e.g. in non-expression contexts).

### Expression-level guards

`lower_guarded_expression` wraps binary/unary ops with a shared fail block.
`emit_type_guard` checks each arg's type matches `param_type()` via a
multi-arm Match. All guards within an expression jump to the same fail_bb.
One Phi at the end merges the result with Undefined.

```
// Expression: a + b * c
//
// lower_guarded_expression sets up fail_bb, then:

Match(b, [UInt→ok, Int→ok, Float→ok], fail_bb)   // type guard for b
Match(c, [UInt→ok, Int→ok, Float→ok], fail_bb)   // type guard for c
t0 = Mul(b_narrowed, c_narrowed)                  // guaranteed numeric inputs
Match(a, [UInt→ok, Int→ok, Float→ok], fail_bb)   // type guard for a
Match(t0, [UInt→ok, Int→ok, Float→ok], fail_bb)  // type guard for mul result
t1 = Add(a_narrowed, t0_narrowed)                 // guaranteed numeric inputs

// ok path continues with t1
// fail path: result = Undefined
// join: Phi(ok→t1, fail→Undefined)
```

When types are known (e.g. both args are UInt constants), `eliminate_dead_match_arms`
removes the guards and `simplify_cfg` merges the blocks — no overhead.

### Narrowing copies (pi-nodes)

After a Match proves a value's type, `emit_narrowing` creates a Copy with the
narrowed TypeSet. This is valid because the Match performed a real runtime check.
The type analysis intersects the Copy's declared type with the source type.

### Undefined semantics

- `Undefined == Undefined` → `Undefined` (not `true`)
- Any operation with Undefined input → `Undefined`
- `Undefined` observed only via `if let` / `match` with `Type(Undefined)` arm
- No `None` literal in the language
- `result_type()` includes Undefined for fallible ops (overflow, div-by-zero)

### What the intrinsic closures check

Each `exec_*` function has a Rust `match` on value types. With type guards,
these checks are redundant — the guard already rejected invalid types.
The `_ => Value::Undefined` arms become unreachable. They will be converted
to `debug_assert!` once the type guard coverage is confirmed complete.

## Per-intrinsic analysis

### Arithmetic: exec_add, exec_sub, exec_mul, exec_div, exec_mod

```
Type guard: Match(a, [UInt, Int, Float]) + Match(b, [UInt, Int, Float])  — emitted
Coercion: mixed types → Convert to common type                          — emitted
Specialization: same-type pairs → direct checked_add etc.                — emitted
Overflow/div-zero → Undefined                                            — in result_type()
```

### exec_neg

```
Type guard: Match(a, [UInt, Int, Float])                                 — emitted
Overflow (i64::MIN) → Undefined                                          — in result_type()
```

### exec_eq

```
Type guard: Match(a, [all defined types]) + Match(b, [all defined types]) — emitted
Infallible with defined inputs                                            — result: Bool
```

### exec_lt

```
Type guard: Match(a, [UInt, Int, Float]) + Match(b, [UInt, Int, Float])  — emitted
Type mismatch → Undefined                                                 — in result_type()
```

### exec_not

```
Type guard: Match(a, [Bool])                                              — emitted
Infallible with Bool input                                                — result: Bool
```

### Bitwise: exec_bitand, exec_bitor, exec_bitxor, exec_bitnot

```
Type guard: Match(a, [UInt]) [+ Match(b, [UInt])]                        — emitted
Infallible with UInt inputs                                               — result: UInt
```

### Shifts: exec_shl, exec_shr

```
Type guard: Match(a, [UInt]) + Match(b, [UInt])                          — emitted
Checked: shift amount >= 64 → Undefined                                   — result: UInt | Undefined
```

### exec_bittest, exec_bitset

```
Type guard: Match(x, [UInt]) + Match(b, [UInt]) [+ Match(v, [Bool])]    — emitted
Bit position >= 64 → Undefined                                           — in result_type()
```

### exec_len

```
Type guard: Match(a, [Array, Map, Text, Bytes, Sequence])                — emitted
  (via lower_guarded_expression in try_lower_intrinsic)
Infallible with collection input                                          — result: UInt
(Sequence: remaining() may return None → Undefined)                      — in result_type()
```

### exec_make_array, exec_make_map

```
Type guard: Match(each arg, [all defined types])                         — emitted
Infallible (MakeMap odd-arg-count is debug_assert — compiler bug)        — result: Array/Map
```

### exec_make_seq

```
Type guard: Match(start, [UInt]) + Match(end, [UInt])                    — emitted
Infallible with UInt inputs                                               — result: Sequence
```

### exec_convert

```
Type guard: Match(value, [UInt, Int, Float])                             — emitted
Checked UInt→Int overflow → Undefined                                    — in result_type()
```

### index_value

```
Type guard: not yet emitted (Index instruction not via emit helpers)
Bounds check: runtime only
Key not found: runtime only
```

## Remaining work

| Priority | Item | Status |
|----------|------|--------|
| **Done** | Expression-level type guards for binary/unary ops | `emit_type_guard` in lowerer |
| **Done** | `result_type()` includes Undefined for fallible ops | Merged `is_fallible` |
| **Done** | Undefined poisons everything (exec_eq, param_type) | All param_types use `defined()` |
| **Done** | Convert `_ =>` arms to `debug_assert!` (guarded ops) | 12 exec functions: eq, mul, div, mod, neg, not, bitand/or/xor/not, shl, shr |
| **Done** | Guard wrapping for `len()`, `collect()`, `append()` | `lower_guarded_expression` in `try_lower_intrinsic` |
| **Done** | Guard wrapping for range expressions (`..`, `..=`) | `lower_guarded_expression` in `lower_range` + `seq_exit` block tracking |
| **Done** | Guard wrapping for for-loop Len | Narrowing copy (exclude Sequence) in `lower_for` + `lower_guarded_expression` in `lower_for_idx` |
| **Done** | Guard wrapping for cast expressions (`as`) | `lower_guarded_expression` in `lower_cast` |
| **Done** | Guard wrapping for compound assignment (`+=` etc.) | `lower_guarded_expression` in `lower_assignment` |
| **P3** | Peephole layer: fuse `Match + Op` into single step | StepKind design in TODO.md |
| **P2** | Type guards for Index instruction | Not yet via emit helpers |
| **P2** | `b != 0` divisor guard for Div/Mod | Domain-specific |
| **P2** | `key < len(base)` bounds guard for Index | Domain-specific |
| **P2** | `b < 64` bit position guard for BitTest/BitSet | Domain-specific |
