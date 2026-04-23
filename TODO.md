# Rill TODO

## Project Overview

Rill is a memory-safe, embeddable scripting language written in Rust.
Architecture: Source → Parser (chumsky) → AST → Lower (operators → IntrinsicOp) → IR (SSA) → Optimize → Compile (closure-threaded) → Execute (flat pc-based loop).

## What's Done

The full compilation and execution pipeline is working end-to-end with 270+ tests passing.

- **Parser** — chumsky-based, implicit return support (`src/ast/parser.rs`)
- **AST** — type definitions (`src/ast/`), `TypeSet` as `u16` bitfield (`src/types.rs`)
- **IR lowering** — AST → pre-SSA IR (`src/ir/`), then mem2reg → SSA (`src/ssa/`)
  - Statement, expression, control flow, pattern destructuring lowering
  - Emit helpers (emit_const, emit_copy, emit_index, emit_call, emit_phi, etc.)
  - Slot-based binding (Assign/Read) — SSA promotion handled by mem2reg pass
  - `with` reference bindings (MakeRef/WriteRef), ref origin tracking
  - Constant expression lowering and compile-time evaluation
  - IntrinsicOp for all operators, `len`, collection construction
  - Compile-time properties in op variants: `Convert(NumericType, ConvertMode)`, `MakeSeq`, `ArraySeq(SliceMode)`
  - Range guard: `start < end` If check before MakeSeq/ArraySeq (reversed → undefined)
  - Inclusive ranges normalized to exclusive via `end + 1` checked Add in lowerer
  - Expression-level type guards (`lower_guarded_expression`, `emit_type_guard`)
  - Extern param type guards (Match guards inserted before constrained calls)
- **Optimizer** — passes in two-phase pipeline (`src/opt/`)
  - Phase 1 (fixpoint): const fold → CSE → copy prop → DCE → ref elision → coercion elision → CFG simplify
  - Phase 2 (type-informed): type analysis → coercion insertion → cast elision → algebraic simplification → non-bool condition folding → dead match arm elimination → re-run Phase 1
  - Interprocedural return type inference + argument type propagation
  - Function monomorphization (up to 4 variants per function)
  - Type mismatch warnings (W009), definedness warnings (W201) via TypeSet
  - Unified type/definedness: Undefined is a type, no separate definedness pass
  - Phase T: tail-call optimization (self-recursive, backward phi-chain detection)
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
  - `ExecResult::Exit(val)` → `Action::Exit` for embedder escape (no IR terminator needed)
- **Diagnostics** — source spans, line:column formatting, error codes (`src/diagnostics.rs`)
- **Docs** — ABNF grammar, design document, stdlib spec, examples, benchmarks

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
- [x] **By-ref parameter passing** — Unified caller-emitted model:
      - Default is by-value (CoW clone — Rc increment). `with` opts into by-reference.
      - Caller emits `MakeRef` for by-ref args, plain value for by-val, at the call site.
      - Compiler is ref-agnostic: uniform `copy_slot_from` for all args.
      - TailCall uses `set_local` (writes through Refs) + `reset_local` (clears locals).
      - Reload instructions provide SSA visibility after by-ref calls.
      - Consistent across function params, for loops, and match arms.
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
    - [ ] `ToCbor` / `FromCbor` for `FunctionRef` (namespace + name)
    - [ ] `ToCbor` / `FromCbor` for `Terminator` (opcode + operands, including TailCall)
    - [ ] `ToCbor` / `FromCbor` for `MatchPattern`
    - [ ] `ToCbor` / `FromCbor` for containers: `Var`, `BasicBlock`
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

### P1 — SSA Construction — Done

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
  - Phase 3: Validate — **done**
    - [x] `bench_ackermann` — was failing due to parser bug in `else if` chains
          (inner `if` result discarded as statement instead of flowing as expression).
          Fixed in parser: `else if` now emits the inner `if` as `else_expr`.
    - [x] `bench_map_operations` — fixed by LowerVar refactor (redundant Copy removal)

### P2 — Optimization

- [x] **Tail-Call Optimization (TCO)** — self-recursive tail calls detected via
      backward phi-chain tracing from Return, rewritten to `Terminator::TailCall`.
      Compiled as param overwrite + `NextBlock(entry)` — no new frame, no stack growth.
      Enables unbounded recursion depth for tail-recursive functions (100K+ tested).
      Scope: self-recursive, by-value args only. See `src/opt/tail_call.rs`.
- [x] **Element type tracking (Layer 1)** — `element_state` in type analysis tracks
      union of all element types per collection VarId. Flows into Index/MakeRef result
      types automatically (e.g., `[1,2,3][i]` → `UInt | Undefined` not `any()`).
      Sources: MakeArray, MakeMap, Append, WriteAccessor, Phi (all-sources union), Copy, Reload.
      Elements never include Undefined; Index adds it for OOB/missing key.
- [ ] **Per-key type tracking (Layer 2)** — `HashMap<Value, TypeSet>` per collection
      VarId for constant keys. Structural typing for maps: `config.timeout` → UInt,
      `config.name` → Text. Only for constant keys (string/integer literals).
      Variable-key access falls back to Layer 1 union type. Enables typed record
      patterns for map-heavy code (DTN config processing).
- [ ] **Function Inlining** — clone callee IR into call site for small pure
      functions. Works best after monomorphization: the inlined clone is
      already type-specialized, so the inlined body folds further via
      const fold + coercion elision. Decision: inline if callee is pure,
      small (< ~10 instructions), and called with known-type args.
- [ ] **Dominator tree + Cytron SSA** — when LICM or GVN is needed, build a
      dominator tree and switch from Braun et al. to Cytron et al. for SSA
      construction. The tree enables three things at once:
      1. LICM (lift loop-invariant code to pre-header)
      2. GVN (more powerful CSE across blocks)
      3. Array bounds checking (recognise `i < len(arr)` guards dominating
         `arr[i]` and mark Index results as defined)
      Braun has no advantage once the tree exists. Single migration, three wins.
      Note: for-loop element bindings currently use explicit narrowing copies
      (`emit_copy(raw, TypeSet::defined())` in `lower_for_idx`, `emit_narrowing`
      in `lower_for_seq`) because the lowerer knows `i < len` holds. With
      dominator-based bounds checking, the type analysis would prove this
      automatically and these manual annotations can be removed.
- [ ] **Dead write-back elimination** — a WriteRef exists but the base value is never
      read after the write-back point. Requires liveness analysis.

### P2 — Architecture

- [x] **Unified type/definedness** — `BaseType::Undefined` in TypeSet,
      `Value::Undefined` at runtime, `Terminator::Guard` → Match,
      `Instruction::Undefined` → `Const { Literal::Undefined }`,
      definedness pass deleted. See `docs/unified_definedness_plan.md`.
- [x] **Clean layer separation** — Lowerer uses emit helpers (emit_const,
      emit_index, emit_call, etc.) for temps, bind/read_var/reassign for
      named variables. emit_guard/emit_match return narrowed VarIds (pi-nodes).
      TypeAnalysis simplified to one TypeSet per VarId (no per-block tracking).
      Copy transfer uses declared type intersection for narrowing.
- [x] **Expression-level undefined guards** — `lower_guarded_expression`
      wraps binary/unary ops with a shared fail block. All guards within
      the expression jump to the same fail_bb. One Phi at the end merges
      result with Undefined. No cascade — inner guards reuse the outer
      fail_bb. `fail_used` check skips Phi when no guards were triggered.
- [x] **`definedness.rs` deleted** — definedness pass fully replaced by TypeSet.
- [x] **Type guards for intrinsic args** — `emit_type_guard` checks args
      match `param_type()` via Match. Fallibility in `result_type()`.
      `is_fallible()` removed — folded into `result_type()` per-arm.

### P1 — SSA Mutation Visibility

- [x] **SSA reload after mutation** — `Instruction::Reload { dest, src }` creates a
      new SSA def after in-place mutation, opaque to mem2reg. Copy propagation does not
      propagate through Reload (barrier). Type analysis uses source type.
  - [x] `Instruction::Reload` added to IR, compiler, all optimizer passes
  - [x] Lowerer emits Reload + Assign after all mutations (WriteAccessor, WriteRef)
  - [x] Lowerer emits Reload + Assign after calls for by-ref (`with`) args

### P1 — Accessor/Ref Model (done)

- [x] **Slot::Accessor** — far pointer into collection elements (`base + key` slot indices).
      `vm.get()` reads through Accessors. `vm.set()` writes through Accessors (SetIndex).
      Composes with Slot::Ref: `Ref → Accessor` chains resolve automatically.
- [x] **Four reference instructions** — clean separation of concerns:
      - `MakeAccessor { dest, base, key }` → creates `Slot::Accessor`
      - `MakeRef { dest, base }` → creates `Slot::Ref` (with path compression)
      - `WriteAccessor { base, key, value }` → direct element write (type-specialized)
      - `WriteRef { ref_var, value }` → write through binding (VM resolves Ref/Accessor)
- [x] **SetIndex removed** — was premature peephole optimisation in the lowerer.
      All collection mutations now go through MakeAccessor + WriteAccessor + Reload.
      The peephole layer (future StepKind) can fuse back to a single closure.
- [x] **build_ref_map/RefMeta removed** — WriteAccessor carries base+key directly.
      WriteRef uses vm.set_local which resolves through Slot types. No tracing.
- [x] **Frame stack separated** — `FrameInfo` moved from `Slot::Frame` to a
      separate `Vec<FrameInfo>`. Removes `Frame` from Slot enum (one fewer variant
      in hot path), eliminates `Box` allocation per call, removes `rotate_right(1)`
      in `call_with_args`. Slot offsets are now 0-based (`slot(VarId) = var.0`).
      Stack size configurable via `VM::with_limits(heap, stack)`.
- [x] **ref_elision updated** for Accessor/Ref split:
      - Ref(Accessor) → Accessor: flattens double indirection
      - Read-only Accessor → Index: removes Slot::Accessor overhead
      - Read-only Ref → Copy: removes Slot::Ref overhead
      - `with` without write-back optimises to same IR as `let`
      - WriteAccessor bases tracked in written_bases (prevents incorrect Ref demotion)

### P2 — Known Issues (resolved)

- [x] **Type analysis Phi convergence** — Fixed: Phi sources default to `none()` (bottom)
      instead of `any()` (top). Standard dataflow bottom-start. The worklist converges
      correctly: `Map ∪ none() = Map` instead of `Map ∪ any() = any()`.
      Loop-carried collections now get type-specialized dispatch.

### P2 — Parser
- [ ] **Optional braces in match arms** — allow `pattern => expr,` in addition to
      `pattern => { stmts; expr }`. The `=>` token disambiguates; trailing `,` or `}`
      delimits the bare expression. Currently `block_body()` always requires braces.

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
    WriteAccessor { base: usize, key: usize, value: usize },
    // terminators
    BranchIf { cond: usize, then_pc: usize, else_pc: usize },
    Jump { pc: usize },
    MatchType { value: usize, arms: Vec<(BaseType, usize)>, default_pc: usize },
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

Optimizer pipeline:
```
Phase 1 (fixpoint):
  Const Fold → CSE → Copy Prop → DCE → Ref Elision → Coercion Elision → CFG Simplify

Phase 2 (type-informed — on simplified CFG):
  Type Analysis → Coercion Insertion → Cast Elision → Algebraic Simplification
    → Non-Bool Condition Folding → Dead Match Arm Elimination → Re-run Phase 1

Phase 3 (post-optimisation):
  W201 definedness warnings (TypeSet-based)

Phase T (tail-call):
  Self-recursive tail call detection → TailCall rewrite → CFG Simplify
```

Undefined is a type (`BaseType::Undefined`). No separate definedness pass.
Type guards (`emit_type_guard`) check both type and definedness via Match.
Expression-level guards (`lower_guarded_expression`) prevent Phi cascade.
See `docs/unified_definedness_plan.md` and `docs/runtime_checks.md`.

## File Map

```
src/
  lib.rs              — Public API: compile(), Program::call(), re-exports
  types.rs            — BaseType, NumericType, ConvertMode, SliceMode, TypeSet (u16 bitfield)
  diagnostics.rs      — Error/warning accumulator with codes (E1xx, W0xx, W2xx)
  externs.rs          — ExternRegistry, Lua-style extern API: fn(&mut VM, usize)
  exec.rs             — VM, Heap, HeapVal, Value, Slot, Float
  ast/
    mod.rs            — Re-exports
    types.rs          — AST node types, Span, Spanned
    parser.rs         — Chumsky-based parser → AST
  ir/
    mod.rs            — Lowerer state, scope/slot management, emit helpers, public lower() API
    types.rs          — IR types: VarId, BlockId, Instruction, IntrinsicOp, Terminator (Jump/If/Match/Return/Unreachable/TailCall)
    program.rs        — Top-level program lowering (constants, functions)
    stmt.rs           — Statement lowering
    expr.rs           — Expression lowering (guarded expressions, binary/unary ops)
    control.rs        — Control flow lowering (if, match, loops, emit_guard/emit_match/emit_narrowing)
    pattern.rs        — Pattern destructuring lowering
    constant.rs       — Constant expression lowering
    const_eval.rs     — Compile-time constant evaluation (intrinsic + extern)
  ssa/
    mod.rs            — SSA module entry
    promote.rs        — Braun et al. (2013) mem2reg pass (Assign/Read → Phi/VarId)
  opt/
    mod.rs            — Optimizer pass runner (Phase 1 fixpoint + Phase 2 type-informed)
    const_fold.rs     — Constant folding
    cse.rs            — Common subexpression elimination
    copy_prop.rs      — Copy propagation (skips narrowing copies)
    dce.rs            — Dead code elimination
    ref_elision.rs    — Ref/Accessor elision (Ref→Copy, Accessor→Index, Ref(Accessor)→Accessor)
    coercion.rs       — Coercion insertion + elision (checked Convert for mixed types)
    cfg_simplify.rs   — CFG simplification (unreachable block removal, block merging)
    type_refinement.rs — Type analysis (per-VarId TypeSet, Match refinement)
    cast_elision.rs   — Identity Convert → Copy
    algebra.rs        — Algebraic simplification (identity, annihilation, strength reduction)
    tail_call.rs      — Tail-call optimization (self-recursive detection + rewrite)
  compile/
    mod.rs            — Compiler: link phase, phi elimination, flat PC executor
    terminator.rs     — compile_terminator, compile_match, match predicate compilation
    specialize.rs     — Type-specialized closures, compile_intrinsic_dispatch
    exec.rs           — Per-op functions (exec_add, exec_eq, etc.), index_value
    tests.rs          — Unit + end-to-end tests (260+)

docs/
  DESIGN.md           — Comprehensive design document
  STDLIB.md           — Standard library specification
  grammar.abnf        — Formal ABNF grammar
  example.txt         — Syntax examples
  benchmarks.rill     — Benchmark functions (fib, ack, tak, primes, binary_trees)
  bytecode_format.md  — Bytecode serialization design (planned)
  runtime_checks.md   — Expression-level guard design
  unified_definedness_plan.md — Unified type/definedness (completed)
```
