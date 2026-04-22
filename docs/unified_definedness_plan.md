# Unified Type/Definedness — Complete

Undefined is a type (`BaseType::Undefined`) in the TypeSet. There is no
separate Definedness lattice or analysis pass.

## What was done

1. `BaseType::Undefined` added to TypeSet (10th type, bit 9)
2. `Value::Undefined` replaces `Option<Value>` throughout
3. `vm.local()` returns `&Value`, `Slot::Uninit` removed
4. `Instruction::Undefined` → `Const { Literal::Undefined }`
5. `Terminator::Guard` → `Match` with `Type(Undefined)` arm
6. Definedness pass deleted (`definedness.rs`)
7. TypeAnalysis simplified to per-VarId (no per-block tracking)
8. Lowerer emit helpers with type narrowing (pi-nodes)
9. Expression-level type guards (`lower_guarded_expression`)
10. `result_type()` includes Undefined for fallible ops
11. All intrinsics require defined inputs (`param_type` excludes Undefined)

## Key design decisions

- **Undefined poisons everything** — like SQL NULL or NaN
- **`undefined == undefined` → `undefined`** (not `true`)
- **Expression-level guards, not per-operation** — prevents Phi cascade
- **Type guards, not definedness guards** — `emit_type_guard` checks
  both type AND definedness via Match against `param_type()`
- **Narrowing copies valid only after Match** — Copy-based narrowing
  without a runtime check is "cheating"
- **`result_type()` is truthful** — includes Undefined for fallible ops
- **`is_fallible()` removed** — folded into `result_type()` directly

## TypeSet naming

| Constructor | Meaning |
|---|---|
| `any()` | Top — all types including Undefined |
| `defined()` | All value types, excludes Undefined |
| `none()` | Bottom — unreachable/dead code |
| `undefined()` | Exactly `{Undefined}` |
| `numeric()`, `bool()`, etc. | Specific types, exclude Undefined |
