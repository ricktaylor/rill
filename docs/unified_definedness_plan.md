# Unified Type/Definedness Implementation Plan

Treat Undefined as `BaseType::Undefined` in the TypeSet, eliminating the
separate Definedness lattice and analysis pass. Guard becomes Match.
`Option<Value>` becomes `Value` with an `Undefined` variant.

## Completed

- **Stage 1**: `BaseType::Undefined`, `Value::Undefined`, `TypeSet::defined()`/`any()`/`undefined()`
- **Stage 2**: exec functions return `Value` not `Option<Value>`
- **Stage 3**: `vm.local()` returns `&Value`, `Slot::Uninit` removed, `set_local_uninit` removed
- **Public API**: `Program::call()` returns `Result<Value>`, `ExecResult::Return(Value)`, `Action::Return(Value)`
- **Cleanup**: `all_defined` flag removed from `compile_intrinsic_dispatch`
- **Stage 4a**: `Literal::Undefined` added, `Instruction::Undefined` removed (replaced with `Const { Literal::Undefined }`)
- **Stage 4b**: `Terminator::Guard` removed, replaced with `Match { arms: [(Type(Undefined), undef_bb)], default: defined_bb }`
- **Critical fix**: `TypeSet::all()` aliased to `any()` (includes Undefined) so type analysis correctly models unknown variables as possibly-undefined. Without this, `eliminate_dead_match_arms` incorrectly killed all definedness guards.
- **Stage 5**: Definedness pass disconnected from optimizer pipeline. `analyze_definedness`, `eliminate_guards`, `check_definedness`, `ParamDefinedness` all removed from the pipeline. Guard elimination handled by `eliminate_dead_match_arms` via TypeSet. Compiler uses `TypeSet::is_defined()` instead of `Definedness::Defined`. If terminator specialization simplified (no unreachable for Undefined conditions). `definedness.rs` file kept as dead code for reference.

## Stage 1: Foundation Types

### types.rs
- Add `BaseType::Undefined` variant (10th type, bit 9 in u16 bitfield)
- Update `BaseType::ALL` array to include Undefined
- Add `Display` arm for Undefined
- Rename `TypeSet::all()` → `TypeSet::defined()` (all value types, excludes Undefined)
- Add `TypeSet::any()` (true top, includes Undefined)
- Update `TypeSet` doc comment — remove "definedness tracked orthogonally" note
- Add `TypeSet::is_defined()` query → `!self.contains(Undefined)`
- Add `TypeSet::undefined()` convenience → `TypeSet::single(BaseType::Undefined)`

### exec.rs (Value)
- Add `Value::Undefined` variant
- Update `Value` Debug, PartialEq, Hash, Clone impls (derive handles most)
- Remove `HeapSize` concern — Undefined has no heap allocation

## Stage 2: VM and Slot Simplification

### exec.rs (Slot)
- Remove `Slot::Uninit` variant — use `Slot::Val(Value::Undefined)` instead
- `Slot::as_value()` no longer returns Option — Undefined is a value
- Or simplify: if Slot is now only `Val | Ref | Frame`, consider whether
  Ref and Frame can be handled differently

### exec.rs (VM)
- `vm.local(offset)` → returns `&Value` not `Option<&Value>`
  - Currently returns `None` for `Uninit` and non-Val slots
  - With Undefined-as-value: returns `&Value::Undefined` for what was Uninit
  - Ref slots: still follow the indirection, return the pointed-to Value
- `vm.set_local_uninit(d)` → `vm.set_local(d, Value::Undefined)`
  - Delete `set_local_uninit` method, replace all 46 call sites
- `vm.push(value)` — no change (Value::Undefined is pushable)
- Stack initialization: fill with `Value::Undefined` instead of `Slot::Uninit`

## Stage 3: Exec Functions

### compile/exec.rs
- All `exec_*` functions: return `Value` instead of `Option<Value>`
  - `None` → `Value::Undefined`
  - `Some(v)` → `v`
- `index_value` → returns `Value` not `Option<Value>`
- `exec_convert` → returns `Value`
- `exec_make_seq`, `exec_array_seq` → return `Value`
- `exec_make_array`, `exec_make_map` → return `Result<Value, ExecError>`

### compile/specialize.rs
- `compile_intrinsic_dispatch`: the `emit!` / `emit_binary!` / `emit_unary!`
  macros simplify — no `match result { Some(v) => set_local, None => set_local_uninit }`
  Just `vm.set_local(d, result)`.
- `all_defined` paths: `unwrap()` calls become direct access since
  `vm.local()` always returns `&Value`. The check becomes
  "does type analysis say this isn't Undefined?" — same query, no Option.
- `try_specialize_convert`, `try_specialize_binary`: closures return Value
  directly, set_local unconditionally.

### compile/mod.rs
- `compile_instruction` for Index, SetIndex, Append: simplify Option handling
- Remove `build_const_uint_map` if not already done (done)
- `all_defined` check: `!type.contains(BaseType::Undefined)` instead of
  `defs.get_at_exit() == Definedness::Defined`

## Stage 4: IR Changes

### ir/types.rs
- `Instruction::Undefined { dest }` — keep as syntactic sugar for
  `Const { dest, value: Literal::Undefined }`. Or add `Literal::Undefined`.
  Decision: add `Literal::Undefined` variant, remove `Instruction::Undefined`.
  One fewer Instruction variant, Const handles it uniformly.
- `Terminator::Guard { value, defined, undefined }` → replace all
  occurrences with `Terminator::Match` using `[(Type(Undefined), undef_bb)]`
  as the arm and `default: defined_bb`. Then delete the Guard variant.

### ir/types.rs (Terminator)
- Delete `Terminator::Guard` variant
- Update `successors()` — Guard arm removed
- All lowerer sites that emit Guard → emit Match with Undefined arm instead

## Stage 5: Delete Definedness Pass

### opt/definedness.rs
- Delete the file entirely
- Remove from `mod.rs` imports and optimizer pipeline

### opt/mod.rs
- Remove `analyze_definedness` calls
- Remove `Definedness` enum re-exports
- Remove definedness-related phases from the optimizer pipeline
- `all_defined` in compiler: query TypeAnalysis for `!contains(Undefined)`

### opt/guard_elim.rs
- Guard elimination logic merges into dead match arm elimination
  (already handles Match arms that type analysis proves unreachable)
- If Guard is gone, the guard_elim pass either:
  - Becomes a no-op (delete it)
  - Or its CFG simplification logic is kept as a general pass

### Diagnostics
- E200/E201 (definedness warnings) — emit from type refinement when
  TypeSet contains Undefined, not from a separate definedness pass
- Same diagnostic codes, different source pass

## Stage 6: Cleanup

- Remove `Definedness` enum from `opt/mod.rs` public API
- Remove `DefinednessAnalysis` type
- Remove `all_defined` parameter from `compile_intrinsic_dispatch` — query
  TypeAnalysis directly for `!contains(Undefined)` on each arg
- Update TODO.md, DESIGN.md, runtime_checks.md
- Update memory

## Call Site Counts (for estimation)

| Pattern | Count | Files |
|---------|-------|-------|
| `TypeSet::all()` | 99 | 17 |
| `Option<Value>` | 38 | 8 |
| `set_local_uninit` | 46 | 3 |
| `Instruction::Undefined` | ~20 | ~8 |
| `Terminator::Guard` | ~15 | ~6 |
| `Definedness::` | ~30 | ~5 |

## Order of Operations

Stages 1-3 can be done without changing the IR or optimizer — purely
runtime/compiler changes. The IR still uses `Instruction::Undefined` and
`Terminator::Guard` internally; they just produce `Value::Undefined`
instead of `Option::None` at runtime.

Stages 4-5 are the IR/optimizer changes — these depend on Stages 1-3
being complete so the runtime can handle the new representation.

Stage 6 is cleanup — can be done incrementally.

Each stage should leave the codebase compiling and all tests passing.
