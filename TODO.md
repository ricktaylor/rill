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
  - Extern param type guards (Match guards inserted before constrained calls)
- **Optimizer** — 11 passes in two-phase pipeline (`src/ir/opt/`)
  - Phase 1 (fixpoint): const fold → CSE → copy prop → definedness → guard elim → CFG simplify → coercion elision → DCE
  - Phase 2 (type-informed): type refinement → coercion insertion (Widen/Undefined) → algebraic simplification → cast elision → ref elision → dead arm elimination → re-run Phase 1
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
  - Sequence type (lazy ranges, zero-copy array slices with mutable flag)
  - For-loop type dispatch (Sequence → SeqNext path, default → index path)
  - For-loop pair binding (`for k, v in map`)
- **Public API** — `compile()`, `Program::call()`, `FunctionHandle` for hot-path (`src/lib.rs`)
- **Externs** — registry with purity tracking, monomorphic variants (`src/externs.rs`)
- **Diagnostics** — source spans, line:column formatting, error codes (`src/diagnostics.rs`)
- **Docs** — ABNF grammar, design document, stdlib spec, examples

All 28 code review issues (CR-1 through CR-27) resolved — see git history.

## Remaining Work

### P1 — Core Functionality

- [ ] **Module/import resolution system** — no multi-file support yet
- [ ] **Standard library**
  - [ ] `std.cbor` (encode/decode)
  - [ ] `std.time` (now, format)
  - [ ] `std.encoding` (hex, base64)
  - [ ] `std.parsing` (parse_int, etc.)
- [ ] **Prelude** — standard utility functions (is_some, is_uint, is_int, ..., default, etc.)
      User-definable functions loaded automatically — not intrinsics.
- [ ] **Public/private function visibility** — structural, not declarative:
      root file functions/constants = public (embedder entry points),
      imported file functions/constants = private (DCE can eliminate unused).
      No `pub` keyword needed. Enables unused-import elimination.
- [ ] **Host sequence support** (`SeqState::Host` variant, defer trait design to embedder API)

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
- [ ] Compiled bytecode serialization format
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
  Type Refinement → Coercion Insertion (generates Match + Widen + Undefined)
    → Definedness (fine — sees explicit Undefined from coercion)
      → Guard Elim → CFG Simplify → Const Fold → CFG Simplify
        → Type-aware closure compilation
```

The coercion pass bridges type analysis and definedness: it transforms type
mismatches into explicit `Undefined` instructions that the existing definedness
analysis can reason about — no new analysis infrastructure needed.

## File Map

```
src/
  lib.rs              — Public API: compile(), Program::call(), re-exports
  compile/
    mod.rs            — Types, public API (compile_program, execute), link phase, compile_function/block/instruction
    terminator.rs     — compile_terminator, compile_match, match predicate compilation
    specialize.rs     — try_specialize_binary/cast/widen, compile_intrinsic_dispatch, type-specialized closures
    exec.rs           — Per-op functions (exec_add etc.), index_value
    tests.rs          — Unit + end-to-end tests
  ast.rs              — AST node types, Span, Spanned
  types.rs            — BaseType, TypeSet
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
      coercion.rs     — Coercion insertion (Widen for mixed types, Undefined for incompatible)
      guard_elim.rs   — Guard elimination + CFG simplification
      definedness.rs  — Definedness analysis
      cast_elision.rs — Identity Cast/Widen → Copy
      copy_prop.rs    — Copy propagation (replace uses, remove dead Copies)
      dce.rs          — Dead code elimination (remove unused instructions)
      algebra.rs      — Algebraic simplification (identity, annihilation, strength reduction)

docs/
  DESIGN.md           — Comprehensive design document
  STDLIB.md           — Standard library documentation
  grammar.abnf        — Formal ABNF grammar
  example.txt         — Syntax examples
  stdlib_prelude.txt  — Prelude function docs
  stdlib_example.txt  — Stdlib usage examples
```
