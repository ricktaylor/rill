# Rill TODO

## Project Overview

Rill is a memory-safe, embeddable scripting language written in Rust.
Architecture: Source → Parser (chumsky) → AST → Lower (operators → IntrinsicOp) → IR (SSA) → Optimize → Compile (closure-threaded) → Execute (flat pc-based loop).

## What's Done

The full compilation and execution pipeline is working end-to-end with 139+ tests passing.

- **Parser** — chumsky-based, implicit return support (`src/parser.rs`)
- **AST** — type definitions, `TypeSet` as `u16` bitfield (`src/ast.rs`, `src/types.rs`)
- **IR lowering** — AST → SSA IR with loop-carried phis (`src/ir/`)
  - Statement, expression, control flow, pattern destructuring lowering
  - `with` reference bindings (MakeRef/WriteRef), ref origin tracking
  - Constant expression lowering and compile-time evaluation
  - IntrinsicOp for all operators, `len`, collection construction
  - Compile-time properties in op variants: `Convert(NumericType, ConvertMode)`, `MakeSeq`, `ArraySeq(SliceMode)`
  - Range guard: `start < end` If check before MakeSeq/ArraySeq (reversed → undefined)
  - Inclusive ranges normalized to exclusive via `end + 1` checked Add in lowerer
  - Extern param type guards (Match guards inserted before constrained calls)
- **Optimizer** — 11 passes in two-phase pipeline (`src/ir/opt/`)
  - Phase 1 (fixpoint): const fold → CSE → copy prop → definedness → guard elim → CFG simplify → coercion elision → DCE
  - Phase 2 (type-informed): type refinement → coercion insertion (Convert/Undefined) → algebraic simplification → cast elision → ref elision → dead arm elimination → re-run Phase 1
  - Interprocedural return type inference + argument type/definedness propagation
  - Function monomorphization (up to 4 variants per function)
  - Type mismatch warnings (W009), definedness diagnostics (E200/E201) with provenance tracking
  - Guarded index suppression (loop guards, length checks, match scrutinees)
- **Compiler** — closure-threaded with type specialization (`src/compile/`)
  - Type-specialized closures (direct `u64::checked_add` etc. when types provably known)
  - Extern monomorphism (variant selection at compile time)
  - Link phase, phi elimination, flat PC executor
- **Runtime** — stack-based VM with heap tracking (`src/exec.rs`)
  - CoW HeapVal, capacity-based heap accounting, configurable limits
  - Sequence type (UInt ranges with exclusive end, zero-copy array slices)
  - For-loop type dispatch (Sequence → SeqNext path, default → index path)
  - For-loop pair binding (`for k, v in map`)
- **Public API** — `compile()`, `Program::call()`, `FunctionHandle` for hot-path (`src/lib.rs`)
- **Externs** — registry with purity tracking, monomorphic variants (`src/externs.rs`)
- **Diagnostics** — source spans, line:column formatting, error codes (`src/diagnostics.rs`)
- **Docs** — ABNF grammar, design document, stdlib spec, examples

All 28 code review issues (CR-1 through CR-27) resolved — see git history.

## Remaining Work

### P1 — Core Functionality

- [ ] **Module system** — `import` for source files, `require` for extern namespaces
  - Phase 1: ExternRegistry namespaces + `require` — **done**
    - [x] `ExternRegistry` restructured: `globals` + `namespaces` maps
    - [x] `register_in(namespace, def)` for namespaced externs
    - [x] `register()` / `register_in()` return `Result<(), RegistryError>`
    - [x] `RegistryError` enum (thiserror): `IntrinsicClash`, `DuplicateGlobal`, `DuplicateInNamespace`
    - [x] `has_namespace()`, `get_in()`, `namespace_iter()`, `globals_iter()`
    - [x] Parser: `require` keyword, `require ident [as (ident / "_")] ;`
    - [x] Parser: `import` takes only quoted string path (removed `ImportPath::Stdlib`)
    - [x] Parser: `import` and `require` support `as _` (merge into root scope)
    - [x] AST: separate `Import` (file path) and `Require` (extern namespace) types
    - [x] `AstProgram.requires` field
    - [x] Lowerer: validate `require` namespaces against ExternRegistry
    - [x] Lowerer: `require_aliases` map for namespace resolution
    - [x] Lowerer: resolve `ns::func()` calls against required extern namespaces
    - [x] Lowerer: resolve `ns::CONST` against required extern namespaces
    - [x] Lowerer: `as _` merges extern functions into root scope (`merged_externs`)
    - [x] Removed IR `Import` type and `IrProgram.imports` (no longer needed)
  - Phase 2: Source file imports
    - [ ] `SourceLoader` trait — `load()` for imports, `preamble()` for standard prelude
    - [ ] `compile()` signature: add `Option<&dyn SourceLoader>` parameter
    - [ ] Import resolution: derive namespace from filename stem
    - [ ] Import resolution: `as name` explicit alias, `as _` merge into root scope
    - [ ] Parse imported source files (recursive — imported files can import)
    - [ ] Cycle detection: error on circular imports
    - [ ] Lower imported functions/constants into namespaced scope
    - [ ] Private imports: each file's imports are invisible to its importers
    - [ ] Diagnostic: "source file not found" (via SourceLoader::load error)
    - [ ] Diagnostic: duplicate namespace alias (import vs import, import vs require)
  - Phase 3: Name clash enforcement — **done**
    - [x] Duplicate function name in same file → error (with note pointing to first definition)
    - [x] Function/constant name vs intrinsic (`len`, `collect`) → error
    - [x] Function/constant name vs global extern → error
    - [x] Function/constant name vs merged extern (`as _`) → error
    - [x] Constant name vs function name → error (with note)
    - [x] Global extern name vs intrinsic → `RegistryError` at registration time
    - [x] Shared `check_name_clash()` helper for consistent error messages
  - Phase 4: Visibility and DCE
    - [ ] Track function origin: root file (public) vs imported file (private)
    - [ ] DCE: imported functions not referenced from root → eliminate
    - [ ] Root file functions always kept (potential embedder entry points)
    - [ ] Unused import warning
- [ ] **Standard prelude** — `STANDARD_PRELUDE` const string of Rill source
      containing is_defined, is_uint, is_int, ..., default, etc.
      Embedder includes via `SourceLoader::preamble()`.
      Not a language feature — an embedder API convenience.
- [ ] **Host sequence support** (`SeqState::Host` variant, defer trait design to embedder API)
- [ ] **Bytecode format** — CBOR serialization of optimized IR (see `docs/bytecode_format.md`)
  - Phase 1: Encoding infrastructure
    - [ ] Add `hardy-cbor` dependency (optional, behind `bytecode` feature flag)
    - [ ] `From<Enum> for u64` / `TryFrom<u64> for Enum` for all discriminated types:
          `BaseType`, `IntrinsicOp`, `Literal`, `ConstValue`, `Instruction`,
          `Terminator`, `MatchPattern`
    - [ ] `ToCbor` / `FromCbor` impls for primitive wrappers: `VarId`, `BlockId`, `TypeSet`
    - [ ] `ToCbor` / `FromCbor` for `Literal`, `ConstValue` (tagged `[type, value]` pairs)
    - [ ] `ToCbor` / `FromCbor` for `Instruction` (opcode + operands array)
    - [ ] `ToCbor` / `FromCbor` for `FunctionRef`, `CallArg` (variable-length arrays)
    - [ ] `ToCbor` / `FromCbor` for `Terminator` (opcode + operands)
    - [ ] `ToCbor` / `FromCbor` for `MatchPattern`
    - [ ] `ToCbor` / `FromCbor` for containers: `Var`, `Param`, `BasicBlock`
    - [ ] `ToCbor` / `FromCbor` for top-level: `Function`, `ConstBinding`, `IrProgram`
    - [ ] `BytecodeError` type for decode errors
  - Phase 2: Top-level API
    - [ ] `bytecode::save(program, debug_info) -> Vec<u8>`
    - [ ] `bytecode::load(bytes) -> Result<(IrProgram, Option<DebugInfo>), BytecodeError>`
    - [ ] Top-level CBOR map: magic, version, functions, constants, debug info
    - [ ] Version validation on load
  - Phase 3: Debug info
    - [ ] `DebugInfo` / `FunctionDebug` types
    - [ ] `extract_debug_info(ir) -> DebugInfo` — capture spans before encoding
    - [ ] `reattach_spans(ir, debug_info)` — restore spans after decoding
    - [ ] `ToCbor` / `FromCbor` for debug info structures
    - [ ] Optional source text inclusion
  - Phase 4: Two-phase optimization
    - [ ] `optimize_pre_link(program, diagnostics)` — optimize without externs
    - [ ] Update `fold_constants` to accept `Option<&ExternRegistry>`
    - [ ] Bytecode emission pipeline: parse → lower → optimize_pre_link → save
    - [ ] Bytecode loading pipeline: load → reattach_spans → optimize → compile
  - Phase 5: Testing
    - [ ] Round-trip tests: IR → save → load → IR (structural equality)
    - [ ] End-to-end: source → bytecode → load → execute (same results as source)
    - [ ] Forward compatibility: unknown top-level keys skipped gracefully
    - [ ] Debug info: present and absent cases

### P1 — SSA Construction — Done (Phases 1–2), 2 ignored tests remain

- [x] **LLVM-style lowering + mem2reg split** — lowerer emits Assign/Read,
      `ssa/promote.rs` implements Braun et al. (2013) mem2reg. Old ad-hoc
      phi insertion (snapshot_scope, merge_branch_bindings, etc.) removed.
  - Phase 1: Pre-SSA IR — **done**
    - [x] `Instruction::Assign { slot, value }` and `Instruction::Read { slot, dest }`
    - [x] Lowerer emits Assign/Read instead of managing VarIds and scopes
    - [x] Old manual phi insertion removed
  - Phase 2: mem2reg pass (Braun et al. 2013) — **done** (`src/ssa/promote.rs`)
    - [x] `read_variable(slot, block)` — recursive predecessor lookup
    - [x] `write_variable` via `current_def` tracking
    - [x] Phi insertion at control flow merge points
    - [x] Trivial phi elimination
  - Phase 3: Validate — **2 ignored tests remain**
    - [ ] `bench_ackermann` — recursive if-else chains produce None
    - [ ] `bench_map_operations` — in-place mutation via SetIndex not visible after for-loop dispatch

### P2 — Optimization

- [ ] **Tail-Call Optimization (TCO)** — rewrite tail calls to parameter overwrite
      + jump to entry. The flat pc-based executor supports this naturally.
- [ ] **Function Inlining** — clone callee IR into call site for small pure
      functions. Works best after monomorphization: the inlined clone is
      already type-specialized, so the inlined body folds further via
      const fold + coercion elision. Decision: inline if callee is pure,
      small (< ~10 instructions), and called with known-type args.
- [ ] **Loop-Invariant Code Motion (LICM)** — lift pure computations with
      loop-external operands to pre-header. Requires loop detection, dominator tree.
- [ ] **Dead write-back elimination** — a WriteRef exists but the base value is never
      read after the write-back point. Requires liveness analysis.

### P2 — Architecture

- [ ] **Unified type/definedness** — treat Undefined as `BaseType::Undefined`
      in the TypeSet, eliminating the separate Definedness lattice and analysis
      pass. See design notes below.
- [ ] **Type guard insertion pass** — emit Guard + Match before intrinsics
      based on `param_type()` constraints. Makes implicit runtime type dispatch
      explicit in IR. Enables peephole fusion. See `docs/runtime_checks.md`.

### P2 — Diagnostics

- [ ] Dead-store warnings for non-ref-backed loop variable mutations
- [ ] Unused variable warnings (from DCE liveness data)

### P2 — Quality

- [ ] Integration test suite
- [ ] Fuzz testing for parser
- [ ] Documentation: API docs, embedding guide

### P3 — Future

- [ ] **StepKind peephole layer** — tagged enum between IR compilation and closure
      generation. Enables multi-instruction fusion (counter increment, accumulator
      update, compare+branch, index+guard, seq advance+guard). Only fuse when
      type-specialized variants exist. See design notes below.
- [ ] **CLI tool** (`rill run script.rl func`, `rill check`, `rill dump --function f`)
- [ ] LSP support
- [ ] Performance benchmarks against Lua, Python (fibonacci, n-body, binary trees, etc.)
- [ ] Domain-specific embedding examples
- [ ] Loop unrolling (small loops with known iteration count)
- [ ] Escape analysis (stack-allocate non-escaping collections)
- [ ] Global Value Numbering (GVN) — more powerful CSE across blocks

## Design Notes

### StepKind Peephole (P3)

Tagged enum between IR compilation and closure generation. Compile IR →
`Vec<StepKind>`, run peephole patterns on the Vec, then convert to closures.
Unlike opaque closures, StepKind is matchable — enabling multi-instruction
fusion that eliminates intermediate slots and closure calls.

Requires TypeAnalysis (already threaded to compiler) so type-specialized
StepKind variants can be emitted (e.g. `AddUU` instead of generic `Add`).

**StepKind sketch:**
```rust
enum StepKind {
    Const { dest: usize, value: Value },
    Copy { dest: usize, src: usize },
    AddUU { dest: usize, a: usize, b: usize },  // UInt + UInt
    AddII { dest: usize, a: usize, b: usize },  // Int + Int
    AddFF { dest: usize, a: usize, b: usize },  // Float + Float
    // ... typed variants for Sub, Mul, Div, Mod, Lt, Eq
    Generic { dest: usize, op: IntrinsicOp, args: Vec<usize> },
    Index { dest: usize, base: usize, key: usize },
    SetIndex { base: usize, key: usize, value: usize },
    // terminators
    BranchIf { cond: usize, then_pc: usize, else_pc: usize },
    Jump { pc: usize },
    Guard { value: usize, defined_pc: usize, undefined_pc: usize },
    Return { value: Option<usize> },
}
```

**Common peephole patterns** (ordered by expected frequency):

_Every loop iteration (highest impact):_

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Counter increment | `i = i + 1` | `Const(1)` + `AddUU(i,c)` + `Copy` | `IncUU { slot, imm: 1 }` | 3→1 |
| Accumulator update | `sum = sum + x` | `AddUU(sum,x)` + `Copy` | `AddAssignUU { dest, src }` | 2→1 |
| Loop condition | `i < len` → branch | `LtUU(i,len)` + `BranchIf` | `BranchLtUU { a, b, t, f }` | 2→1 |
| Array element read | `arr[i]` with guard | `Index` + `Guard` | `IndexGuard { dest, base, key, fail }` | 2→1 |
| Seq advance | `SeqNext` + guard | `SeqNext` + `Guard` | `SeqNextGuard { dest, seq, fail }` | 2→1 |

_Most functions (moderate impact):_

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Const + binop | `x + 5` | `Const(5)` + `AddUU(x,c)` | `AddImmUU { dest, src, imm: 5 }` | 2→1 |
| Compare + branch | `if x == 0` | `EqUU(x,c)` + `BranchIf` | `BranchEqUU { a, b, t, f }` | 2→1 |
| Negate + branch | `if !cond` | `Not(c)` + `BranchIf` | `BranchIf` with swapped targets | 2→1 |
| Copy-to-self | SSA artifact | `Copy(x, x)` | eliminated | 1→0 |

_Write-back paths (ref-backed mutations):_

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Compute + write-back | `x += 1` (ref) | `AddUU` + `WriteRef` + `Copy` | `AddWriteRefUU { ... }` | 3→1 |
| Const array literal | `[1, 2, 3]` | `Const` × 3 + `MakeArray` | `MakeArrayConst { values }` | 4→1 |

_Destructure-collect patterns (sub-slicing):_

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Pop last | `let [..r, last] = a; a = collect(r)` | `ArraySeq` + `Collect` | `ArraySliceCopy { dest, src, start, end }` | 2→1 (one memcpy) |
| Shift first | `let [first, ..r] = a; a = collect(r)` | `ArraySeq` + `Collect` | `ArraySliceCopy { dest, src, start, end }` | 2→1 (one memcpy) |
| Sub-slice | `let [_, ..mid, _] = a; collect(mid)` | `ArraySeq` + `Collect` | `ArraySliceCopy { dest, src, start, end }` | 2→1 (one memcpy) |

**Decision heuristic:** Only fuse when type-specialized variants exist
(TypeAnalysis proves single type). Generic-typed sequences stay as
separate steps — fusion with runtime dispatch would be slower than
the current closure-per-instruction approach.

### Type-Specialized Compilation (completed)

Two-phase definedness model:
```
Phase 1 (coarse — before type info):
  Const Fold → Definedness (coarse) → Diagnostics → Guard Elim → CFG Simplify

Phase 2 (type-informed — on simplified CFG):
  Type Refinement → Coercion Insertion (generates Match + Convert + Undefined)
    → Definedness (fine — sees explicit Undefined from coercion)
      → Guard Elim → CFG Simplify → Const Fold → CFG Simplify
        → Type-aware closure compilation
```

The coercion pass bridges type analysis and definedness: it transforms type
mismatches into explicit `Undefined` instructions that the existing definedness
analysis can reason about — no new analysis infrastructure needed.

### Unified Type/Definedness (P2)

Undefined is really just a type. The separate `Definedness` lattice
(`Defined | MaybeUndefined | Undefined`) and its dedicated analysis pass
can be replaced by adding `BaseType::Undefined` to the TypeSet:

```
Definedness::Defined        →  !type.contains(Undefined)
Definedness::MaybeUndefined →  type.contains(Undefined) && type.len() > 1
Definedness::Undefined      →  type == TypeSet::single(Undefined)
Guard { defined, undefined } →  Match { arms: [(Undefined, undef_bb)], default: ok_bb }
Instruction::Undefined      →  Instruction::Const { Literal::Undefined }
all_defined (compiler)      →  !type.contains(Undefined)  (same query as type analysis)
```

TypeSet naming:
- `TypeSet::any()` — true top, all types including Undefined
- `TypeSet::defined()` — any value type, excludes Undefined (replaces current `all()`)
- `TypeSet::numeric()` etc. — excludes Undefined (implies defined)

Runtime: `Option<Value>` becomes `Value` with an `Undefined` variant.
Slots are always populated. The `all_defined` optimization becomes
"type analysis proved no Undefined in the set" — the same pass that
already narrows types.

This eliminates: the `Definedness` enum, the `DefinednessAnalysis` pass,
the `Guard` terminator variant, and the `Instruction::Undefined` variant.
Guard becomes Match. Undefined becomes Const. One analysis instead of two.

## File Map

```
src/
  lib.rs              — Public API: compile(), Program::call(), re-exports
  compile/
    mod.rs            — Types, public API (compile_program, execute), link phase, compile_function/block/instruction
    terminator.rs     — compile_terminator, compile_match, match predicate compilation
    specialize.rs     — try_specialize_binary/convert, compile_intrinsic_dispatch, type-specialized closures
    exec.rs           — Per-op functions (exec_add etc.), index_value
    tests.rs          — Unit + end-to-end tests
  ast.rs              — AST node types, Span, Spanned
  types.rs            — BaseType, NumericType, ConvertMode, RangeEnd, TypeSet
  parser.rs           — Chumsky-based parser -> AST
  externs.rs         — ExternRegistry, Lua-style extern API: fn(&mut VM, usize)
  diagnostics.rs      — Error/warning accumulator with codes
  exec.rs             — VM, Heap, HeapVal, Value, Slot, Float
  ir/
    mod.rs            — Lowerer state, scope management, public lower() API
    types.rs          — IR types: VarId, BlockId, Instruction, IntrinsicOp, Terminator, etc.
    program.rs        — Top-level program lowering (constants, functions)
    stmt.rs           — Statement lowering
    expr.rs           — Expression lowering
    control.rs        — Control flow lowering (if, match, loops)
    pattern.rs        — Pattern destructuring lowering
    constant.rs       — Constant expression lowering
    const_eval.rs     — Compile-time constant evaluation (intrinsic + extern)
    opt/
      mod.rs          — Optimizer pass runner
      const_fold.rs   — Constant folding pass
      ref_elision.rs  — Ref elision (MakeRef → Copy/Index, chain shortening)
      type_refinement.rs — Type set refinement
      coercion.rs     — Coercion insertion (checked Convert for mixed types, Undefined for incompatible)
      guard_elim.rs   — Guard elimination + CFG simplification
      definedness.rs  — Definedness analysis
      cast_elision.rs — Identity Convert → Copy
      copy_prop.rs    — Copy propagation (replace uses, remove dead Copies)
      dce.rs          — Dead code elimination (remove unused instructions)
      algebra.rs      — Algebraic simplification (identity, annihilation, strength reduction)

docs/
  DESIGN.md           — Comprehensive design document
  STDLIB.md           — Standard library documentation
  grammar.abnf        — Formal ABNF grammar
  example.txt         — Syntax examples
```
