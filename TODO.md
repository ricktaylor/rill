# Rill TODO

## Project Overview

Rill is a memory-safe, embeddable scripting language written in Rust.
Architecture: Source → Parser (chumsky) → AST → Lower (operators → IntrinsicOp) → IR (SSA) → Optimize → Compile (closure-threaded) → Execute (flat pc-based loop).

## Current Status (per README + code inspection)

### Complete
- Grammar specification (ABNF) — `docs/grammar.abnf`
- Full parser with implicit return support — `src/parser.rs`
- AST and type definitions — `src/ast.rs`, `src/types.rs` (TypeSet as u16 bitfield)
- VM core with stack/heap tracking — `src/exec.rs`
- Heap tracking system (CoW HeapVal, capacity-based, limit-checked)
- Builtin registry for host-provided extern functions — `src/builtins.rs`
- IntrinsicOp: all operators, len, collection construction are intrinsics (not builtins)
  Runtime in `compile.rs::exec_intrinsic`, const-eval in `ir/const_eval.rs::eval_intrinsic_const`
- Diagnostics system with source spans — `src/diagnostics.rs`
- IR lowering (AST → SSA IR) with loop-carried phis — `src/ir/`
- Optimizer passes — const fold, definedness, guard elim, CFG simplify, type refinement
- Type refinement is intrinsic-aware: refines result types based on operand types
  (e.g. Add(UInt, UInt) → UInt, not generic numeric). Uses promotion lattice.
- Type mismatch warnings (W009): detects intrinsic ops with incompatible operand types
  that will always produce undefined (e.g. `"hello" + 5`, `!42`, `len(true)`)
- Definedness diagnostics (E200/E201): warns on use of undefined/maybe-undefined values
  with provenance tracking (traces back to originating call/index operation)
- Closure-threaded compiler with link phase — `src/compile.rs`
- Flat pc-based executor — 123 end-to-end tests passing
- Sequence type (lazy ranges, zero-copy array slices with mutable flag)
- For-loop pair binding: `for k, v in map`
- Public API: `compile()`, `Program::call()`, `FunctionHandle` for hot-path
- Source location utilities: `span_to_line_col()`, `LineCol`

### Not Yet Started
- [x] Implement sequence intrinsics at runtime: `MakeSeq`, `ArraySeq`, `SeqNext`
- [x] For-loop sequence path (SeqNext-based loop for Sequence/Range iterables)
- [x] For-loop type dispatch — Match on iterable type: Sequence → SeqNext path,
      default → index path. Both paths lower body independently, outer variables
      merged with Phis at join. Optimizer collapses Match when type is known.
- [ ] Dead-store warnings for mutations to non-ref-backed loop variables
- [ ] Host sequence support (`SeqState::Host` variant)
- [ ] Public/private function visibility — structural, not declarative:
      root file functions/constants = public (embedder entry points),
      imported file functions/constants = private (DCE can eliminate unused).
      No `pub` keyword needed. Enables unused-import elimination.
- [ ] Prelude: standard utility functions (is_some, is_uint, is_int, ..., default, etc.)
      User-definable functions loaded automatically — not intrinsics.
- [ ] CBOR encode/decode integration
- [ ] Comprehensive standard library (std.time, std.cbor, std.encoding, std.parsing)
- [ ] Module/import system implementation
- [x] `with` (reference) binding semantics in IR — Phase 1 complete:
      MakeRef (key: Option<VarId>) + WriteRef instructions. Lowerer emits
      MakeRef for `with` bindings, WriteRef on assignment to ref-backed vars.
      Compiler resolves WriteRef to SetIndex (element) or slot write (whole-value).
      Ref origins tracked in scoped HashMap. Optimizer passes updated.
- [x] `if with` / match arm ref tracking — lower_if_pattern takes ref_origin,
      emits MakeRef for element access in Reference mode, propagates origins
- [x] Ref elision pass (`ir/opt/ref_elision.rs`) — runs in fixpoint loop:
      - Read-only element refs demoted to Index (no WriteRef → no ref needed)
      - Read-only whole-value refs demoted to Copy (base never written → no Slot::Ref needed)
      - Ref chain shortening (MakeRef through MakeRef → skip to resolved base)
- [ ] `with` reference semantics — future:
      - Dead write-back elimination (optimizer can see WriteRef, remove when unused)
      - Ref-backed loop variable dead-store warnings
- [ ] Optimizer: dead code elimination pass

## Code Review Fixes

Issues identified during code review, ordered by priority.

### Critical — Bugs

- [x] **CR-1: Short-circuit AND/OR phi uses wrong block ID** `src/ir/expr.rs`
  Fixed: capture `from_left = self.current_block` before `finish_block` and use
  it in phi sources instead of `BlockId(right_block.0.wrapping_sub(1))`.

- [x] **CR-2: `resolve()` can infinite-loop on circular refs** `src/exec.rs`
  Fixed: converted from recursion to iteration with `MAX_REF_DEPTH` (64) limit.

- [x] **CR-3: Const eval uses wrong namespace separator** `src/ir/constant.rs`
  Fixed: use `::` separator and add `core::` prefix for unqualified names,
  matching `lower_function_call` logic.

- [x] **CR-4: `FunctionRef` naming inconsistent between call sites**
  Fixed: originally added `FunctionRef::core(name)` constructor. Later
  superseded by IntrinsicOp refactor — operators now emit
  `Instruction::Intrinsic` via `emit_binary_intrinsic`/`emit_unary_intrinsic`,
  `FunctionRef::core()` is no longer used.

- [x] **CR-5: `match` lowering is broken** `src/ir/control.rs`
  Fixed: reuses `lower_if_pattern` for each arm with `next_bb` as the failure
  target. Linear chain of pattern checks — correct if not optimal (decision
  trees are a future optimization). Respects `binding_is_value` for ref/value mode.

- [x] **CR-6: `for` loop lowering is non-functional** `src/ir/control.rs`
  Fixed: implemented index-based iteration using `Len` and `Lt` intrinsics.
  Handles both single and pair bindings. Respects `binding_is_value`.
  Later updated for new `for k, v in map` syntax and Sequence type.

- [x] **CR-7: Range lowering is non-functional** `src/ir/control.rs`
  Fixed: lowers to `MakeSeq` intrinsic producing a Sequence value.

- [x] **CR-13: `Value::is_empty()` wrong for scalars** `src/exec.rs`
  Fixed: returns `false` for scalars (no `len`), `true` only for empty
  collections/sequences.

### Significant — Correctness / Robustness

- [x] **CR-8: `dummy_span()` used everywhere in IR lowering**
  Fixed: added `lower_stmt(Stmt)` and `lower_expr(Expr)` wrappers that set
  `current_span` from AST spans. Updated all top-level call sites. Replaced
  all `dummy_span()` in lowering code with `self.current_span`. Test code
  in `opt/` modules retains `dummy_span()` (correct — synthetic IR).
  Also added `span_to_line_col()`, `offset_to_line_col()`, and `LineCol`
  utilities for embedders to convert byte offsets to line:column.

- [x] **CR-9: Pattern lowering silently ignores critical patterns**
  Fixed in both `pattern.rs` (unconditional let/with) and `control.rs` (conditional
  if-let/match):
  - `Pattern::Type`: emits Match terminator, binds inner on match, undefined on mismatch
  - `Pattern::Map`: indexes by literal/variable key, binds value patterns
  - `..rest`: produces a zero-copy Sequence via `ArraySeq` intrinsic.
    `SeqState::ArraySlice` has a `mutable` flag controlled by binding mode:
    `let` → immutable (by-value iteration), `with` → mutable (write-back to source).
  - `after` patterns: indexes from end using `len - after.len() + i`

- [x] **CR-10: `HeapSize` undercounts allocations** `src/exec.rs`
  Fixed: use `capacity()` instead of `len()` for Vec, String, IndexMap.

- [x] **CR-11: `lib.rs` has no public API** `src/lib.rs`
  Fixed: opaque `Program` wrapper, single `compile()` entry point,
  re-exports of key types. Internal modules (`ast`, `ir`, `parser`) stay
  private. Renamed `ast::Program` → `AstProgram`, `ir::Program` → `IrProgram`
  to avoid name collisions with the public `Program` type.

- [x] **CR-12: `ExecError` has no `Display` or `Error` impl** `src/exec.rs`
  Fixed: added `thiserror::Error` derive with human-readable messages.

- [x] **CR-14: `Diagnostics::into_result` loses warnings on success** `src/diagnostics.rs`
  Fixed: returns `Result<(T, Diagnostics), Diagnostics>` — warnings preserved
  in the Ok tuple. Added test for warning preservation.

### Code Quality — Duplication / Efficiency

- [x] **CR-15: Assignment lowering has ~180 lines of duplicated code** `src/ir/stmt.rs`
  Fixed: extracted `lower_indexed_assignment(base, key, op, value)` helper.
  ArrayAccess and MemberAccess now call it with 2 lines each.

- [x] **CR-16: Operator-to-builtin mapping duplicated** `src/ir/expr.rs` + `src/ir/constant.rs`
  Fixed: added `intrinsic_op()` methods on `BinaryOperator`, `UnaryOperator`,
  and `AssignmentOp` AST enums. All lowering sites use the shared methods.
  Old `builtin_name()` methods removed.

- [x] **CR-17: `terminator_successors()` reimplemented** `src/ir/opt/type_refinement.rs`
  Fixed: removed duplicate, uses `block.terminator.successors()` directly.

- [x] **CR-18: `TypeSet` uses `BTreeSet` for 9 variants** `src/types.rs`
  Fixed: replaced with `u16` bitfield. TypeSet is now `Copy` (2 bytes, no heap).
  All constructors and set operations are `const`. Custom `Debug` impl shows
  type names. `iter()` filters over `BaseType::ALL`.

- [x] **CR-19: `Identifier` is noisy to use** `src/ast.rs`
  Fixed: added `Deref<Target=str>` and `Display` impls. New code can use
  `&name` and `format!("{}", name)` naturally. Existing `.0` accesses still
  work and can be cleaned up incrementally.

### Round 2 — Fresh Review Findings

#### Must Fix

- [x] **CR-20: Unchecked indexing in `ret_val` and `bind_param`** `src/exec.rs`
  Fixed: `ret_val` uses `get_mut()` instead of direct indexing. `bind_param`
  validates `slot < stack.len()` before access.

- [x] **CR-21: Break values silently discarded** `src/ir/stmt.rs`, `src/ir/control.rs`
  Fixed: `break value` now pushes `(block_id, var_id)` to `LoopContext.break_values`.
  `lower_loop` and `lower_while` use break values in a Phi node at the exit block.

- [x] **CR-22: For-loop phi patching can silently fail** `src/ir/control.rs`
  Fixed: replaced `if let` chain with `.expect()` calls that panic with clear
  messages if the header block, instruction index, or Phi variant is missing.

- [x] **CR-23: `debug_assert` on `frame_size` should be runtime check** `src/exec.rs`
  Fixed: changed to runtime `if frame_size < 1 { return Err(StackOverflow) }`.

#### Should Fix

- [x] **CR-24: Array/Map literals in patterns produce misleading false** `src/ir/control.rs`
  Fixed: emits E105_InvalidPattern diagnostic before returning fallback value.

- [x] **CR-25: Map pattern silently skips unsupported key patterns** `src/ir/control.rs`
  Fixed: emits E105_InvalidPattern diagnostic on unsupported key patterns.

- [x] **CR-26: No compile-time guard on BaseType variant count** `src/types.rs`
  Fixed: added `assert!((self as u16) < 16)` in `bit()` — panics at const-eval
  time if too many variants are added.

- [x] **CR-27: Sequence case implicit in const_fold pattern matching** `src/ir/opt/const_fold.rs`
  Fixed: added explicit `(BaseType::Sequence, _) => false` with comment explaining
  that sequences are lazy runtime types with no ConstValue representation.

## Task Backlog

### P0 — Critical Path (needed for end-to-end execution)
- [x] Bridge IR to VM execution — closure-threaded compiler in `src/compile.rs`
- [x] End-to-end tests: 139 tests passing — constants, arithmetic, variables,
      if/else, while, loop/break, recursion, short-circuit logic, builtins,
      match patterns (literals, wildcards, types, guards, if-let, array destructure),
      for-loops (array sum, index pairs, break, continue, nested, empty, let binding)
- [x] Parser: implicit return via BlockItem post-processing
- [x] Verify match/pattern execution correctness (9 tests)
- [x] Verify for-loop execution correctness (7 tests)
- [x] Proper SSA with loop-carried phis (while, loop, for)
- [x] Closure-threaded compiler with link phase, phi elimination, flat pc executor
- [x] FunctionHandle API for hot-path execution (no HashMap lookup per call)

### P1 — Core Functionality
- [x] Reference tracking in IR — `with` binding write-back via MakeRef + WriteRef
- [x] Sequence runtime: `MakeSeq`, `ArraySeq`, `SeqNext` in `exec_intrinsic`
      `Len` extended for Sequence. `VM::seq_next()` for borrow-safe mutation.
- [x] For-loop sequence path: `lower_for_seq` uses SeqNext + Guard for range iteration
- [x] For-loop type dispatch: Match(Sequence → seq path, default → idx path),
      outer vars merged with Phis. Optimizer collapses when type is known.
- [ ] Host sequence support (`SeqState::Host` variant, defer trait design to embedder API)
- [ ] Module/import resolution system
- [ ] Standard library: `std.cbor` (encode/decode)
- [ ] Standard library: `std.time` (now, format)
- [ ] Standard library: `std.encoding` (hex, base64)
- [ ] Standard library: `std.parsing` (parse_int, etc.)

### P2 — Optimization Passes

#### Type-Specialized Compilation (Phase 2 of intrinsic refactor)

Operators are now `IntrinsicOp` (Phase 1 complete). Type refinement is
intrinsic-aware — it refines result types using the numeric promotion lattice
and detects guaranteed-undefined operations (W009 warnings).

Phase 2 uses the refined type info to generate explicit guard/coercion/operation
sequences at the IR level, then lets the existing optimizer pipeline (guard
elimination, CFG simplify, const fold) collapse them when types are provably known.

**Key insight:** specialization happens in the IR using existing control flow
primitives (Match, Guard, Phi), not in a separate StepKind layer. The existing
optimizer handles all the simplification — no new infrastructure needed beyond
`Widen` and the coercion insertion pass.

**Completed prerequisites:**
- [x] IntrinsicOp with `is_fallible()`, `result_type()`, `param_type()` methods
- [x] `result_type_refined()` — narrows result types based on operand types
      (e.g. Add(UInt, UInt) → UInt, Add(UInt, Int) → Int, Add(UInt, Float) → Float)
- [x] `numeric_result_type()` / `promote_union()` — promotion lattice logic
- [x] W009 type mismatch warnings — detects ops where operand types guarantee undefined
- [x] Type refinement pass uses `result_type_refined()` for intrinsics

**Two-phase definedness model:**
```
Phase 1 (coarse — before type info):
  Const Fold → Definedness (coarse) → Diagnostics → Guard Elim → CFG Simplify
  [all DONE]

Phase 2 (type-informed — on simplified CFG):
  Type Refinement (DONE) → Coercion Insertion (generates Match + Widen + Undefined)
    → Definedness (fine — sees explicit Undefined from coercion)
      → Guard Elim → CFG Simplify → Const Fold → CFG Simplify
        → Type-aware closure compilation
```

The coercion pass bridges type analysis and definedness: it transforms type
mismatches into explicit `Undefined` instructions that the existing definedness
analysis can reason about — no new analysis infrastructure needed.

**Example:** `v3 = Add(v1, v2)` where `v1: {UInt, Int}`, `v2: {UInt}`:
```
Match v1 {
    UInt → block_uu,
    Int  → block_iu,
    default → block_undef,
}
block_uu:                            // v1: UInt, v2: UInt
    v3a = Intrinsic(Add, [v1, v2])   // both UInt → compiler emits checked_add
    Jump → join
block_iu:                            // v1: Int, v2: UInt
    v4 = Intrinsic(Widen, [v2])      // UInt → Int
    v3b = Intrinsic(Add, [v1, v4])   // both Int → compiler emits checked_add
    Jump → join
block_undef:
    v3c = Undefined
    Jump → join
join:
    v3 = Phi(block_uu: v3a, block_iu: v3b, block_undef: v3c)
```

If TypeAnalysis already proves `v1: {UInt}`, guard elimination collapses the
Match to `Jump → block_uu`, CFG simplify merges the blocks, and we're left
with just `v3 = Intrinsic(Add, [v1, v2])` — both args provably UInt, no guards.

**Steps:**

- [x] **Add `Widen` to IntrinsicOp** — explicit numeric type coercion.
      `Widen(value, target)` where target is UInt encoding BaseType discriminant
      (2=Int, 3=Float). Const-eval folds `Widen(Const(42_u64), 2)` → `Const(42_i64)`.
      Runtime handles UInt→Int (checked, overflow→undefined), UInt→Float, Int→Float.
      UInt→Int overflow (u64::MAX > i64::MAX) returns undefined. `is_fallible()` = true.

- [x] **Coercion insertion pass** (`ir/opt/coercion.rs`) — runs after type
      refinement. For each binary arithmetic intrinsic (Add/Sub/Mul/Div/Mod/Lt):
      - Same known type → no change (monomorphic)
      - Mixed known types → insert Widen for narrower operand
      - Incompatible types → replace with Undefined
      - Unknown/multi-type → leave as-is (runtime dispatch)
      After insertion, re-runs Phase 1 fixpoint loop on the expanded IR:
      const fold collapses Widen(Const), definedness sees Undefined, guard elim
      + CFG simplify clean up dead branches.
      Future: partially-known types (e.g. {UInt,Int}) → generate Match dispatch.

- [x] **Thread TypeAnalysis to closure compiler** — `compile_function` runs
      `analyze_types`, passes result through `compile_block` → `compile_instruction`.
      `try_specialize_binary` checks if both operands are provably same single type,
      emits direct `u64::checked_add` / `i64::checked_add` / `f64` closures instead
      of the 10-way `exec_intrinsic` dispatch. Covers Add/Sub/Mul/Div/Mod/Lt/Eq.

- [x] **Const-fold Widen** — `Widen(Const(42_u64))` where target is Int folds
      to `Const(42_i64)`. Handled by `eval_intrinsic_const`. The Phase 1 re-run
      after coercion insertion picks this up automatically.

- [x] **Redundant coercion elimination** (`elide_coercions` in coercion.rs) —
      runs in Phase 1 fixpoint loop. Chain collapsing: Widen(Widen(x, _), Float)
      → Widen(x, Float). Identity elimination: Widen(v, T) where v already
      produced by Widen(_, T) → Copy. No TypeAnalysis needed.

#### IR-Level (SSA)

- [ ] **Type-Driven Dead Arm Elimination** — use `TypeAnalysis` to prune Match
      arms where `TypeSet ∩ arm_type = ∅`. A Match with one surviving arm becomes
      a Jump. Feeds into CFG simplification → DCE. ~30 lines.

- [ ] **Dead Code Elimination (DCE)** — remove instructions whose dest VarId is
      never used. Iterate until stable. Respect purity: keep impure Calls even if
      result unused. ~50-80 lines.

- [ ] **Copy Propagation** — if `x = Copy(y)`, replace all uses of `x` with `y`.
      Straightforward in SSA.

- [ ] **Common Subexpression Elimination (CSE)** — reuse results of identical pure
      operations. Purity checking via `IntrinsicOp::is_fallible()` and
      `BuiltinMeta.purity`.

- [ ] **Algebraic Simplification** — `x + 0 → x`, `x * 1 → x`, `x * 0 → 0`,
      `x - x → 0`, `!!x → x`, `x && true → x`, `x || false → x`.

- [ ] **Loop-Invariant Code Motion (LICM)** — lift pure computations with
      loop-external operands to pre-header. Requires loop detection, dominator tree.

- [ ] **Interprocedural Type Propagation** — analyze call sites and propagate
      argument types into callee parameter TypeSets. Extends the existing
      per-function `analyze_types` by seeding parameters from callers instead
      of defaulting to `all()`. Single pass over the call graph. When every
      call to `fn process(x)` passes UInt, the analysis proves `x: {UInt}`
      and the entire function body specializes via `try_specialize_binary`.

- [ ] **Function Monomorphization** — clone functions per call-site type
      signature. `process(UInt)` and `process(Int)` become two separate
      compiled functions, each fully specialized. Generates more code but
      zero runtime dispatch in the clones. Natural companion to inlining:
      very small functions get inlined directly (avoiding the call overhead),
      while larger functions get monomorphized (specialized but not inlined).
      Decision heuristic: below N instructions → inline, above → monomorphize
      if types are known, otherwise leave generic.

- [ ] **Tail-Call Optimization (TCO)** — rewrite tail calls to parameter overwrite
      + jump to entry. The flat pc-based executor supports this naturally.

- [ ] **Function Inlining** — clone callee IR into call site for small pure
      functions. Works best after monomorphization: the inlined clone is
      already type-specialized, so the inlined body folds further via
      const fold + coercion elision. Decision: inline if callee is pure,
      small (< ~10 instructions), and called with known-type args.

#### Diagnostics

- [ ] Dead-store warnings for non-ref-backed loop variable mutations
- [ ] Unused variable warnings (from DCE liveness data)

### P2 — Quality

- [ ] Integration test suite
- [ ] Fuzz testing for parser
- [ ] Documentation: API docs, embedding guide

### P3 — Future

- [ ] **StepKind intermediate for peephole** — tagged enum between IR
      compilation and closure generation. Compile IR → `Vec<StepKind>`,
      run peephole patterns on the Vec, then convert `StepKind` → closures.
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

      **Implementation order:**
      1. Define `StepKind` enum with typed variants
      2. Compile IR → `Vec<StepKind>` (replaces current direct-to-closure)
      3. Peephole pass: sliding window over adjacent StepKinds, apply fusions
      4. Convert `StepKind` → closures (final step, same as today but from enum)

- [ ] Compiled bytecode serialization format
- [ ] CLI tool (`rill` binary) — compile and execute scripts from the command
      line. `rill run script.rl func` executes a named function with data from
      stdin (CBOR/JSON) or `--arg`. `rill check script.rl` for compile-only
      diagnostics. `rill dump script.rl --function f` for IR inspection.
      Not a REPL — the language is function-oriented with no top-level state.
- [ ] LSP support
- [ ] Domain-specific embedding examples
- [ ] Loop unrolling (small loops with known iteration count)
- [ ] Escape analysis (stack-allocate non-escaping collections)
- [ ] Global Value Numbering (GVN) — more powerful CSE across blocks

## File Map

```
src/
  lib.rs              — Public API: compile(), Program::call(), re-exports
  compile.rs          — Link phase, closure compilation, exec_intrinsic, phi elimination, flat pc executor
  ast.rs              — AST node types, Span, Spanned
  types.rs            — BaseType, TypeSet
  parser.rs           — Chumsky-based parser -> AST
  builtins.rs         — BuiltinRegistry for host-provided extern functions (empty by default)
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
    const_eval.rs     — Compile-time constant evaluation (intrinsic + builtin)
    opt/
      mod.rs          — Optimizer pass runner
      const_fold.rs   — Constant folding pass
      ref_elision.rs  — Ref elision (MakeRef → Copy/Index, chain shortening)
      type_refinement.rs — Type set refinement
      coercion.rs     — Coercion insertion (Widen for mixed types, Undefined for incompatible)
      guard_elim.rs   — Guard elimination
      definedness.rs  — Definedness analysis

docs/
  DESIGN.md           — Comprehensive design document (65k)
  STDLIB.md           — Standard library documentation
  grammar.abnf        — Formal ABNF grammar
  example.txt         — Syntax examples
  stdlib_prelude.txt  — Prelude function docs
  stdlib_example.txt  — Stdlib usage examples
```
