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
- Closure-threaded compiler with link phase — `src/compile.rs`
- Flat pc-based executor — 123 end-to-end tests passing
- Sequence type (lazy ranges, zero-copy array slices with mutable flag)
- For-loop pair binding: `for k, v in map`
- Public API: `compile()`, `Program::call()`, `FunctionHandle` for hot-path
- Source location utilities: `span_to_line_col()`, `LineCol`

### Not Yet Started
- [ ] Implement sequence intrinsics at runtime: `MakeSeq`, `ArraySeq` (currently return None)
- [ ] Add `SeqNext` intrinsic for sequence consumption in for-loops
- [ ] For-loop type dispatch (Match on iterable type for unknown types)
- [ ] For-loop sequence path (SeqNext-based loop for Sequence type)
- [ ] Dead-store warnings for mutations to non-ref-backed loop variables
- [ ] Host sequence support (`SeqState::Host` variant)
- [ ] Public/private function visibility — structural, not declarative:
      root file functions/constants = public (embedder entry points),
      imported file functions/constants = private (DCE can eliminate unused).
      No `pub` keyword needed. Enables unused-import elimination.
- [ ] CBOR encode/decode integration
- [ ] Comprehensive standard library (std.time, std.cbor, std.encoding, std.parsing)
- [ ] Module/import system implementation
- [ ] `with` (reference) binding semantics in IR
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
  Fixed: added `FunctionRef::core(name)` constructor. Converted all internal
  builtin call sites in `expr.rs` and `stmt.rs` to use it. Also refactored
  `emit_binary_call`/`emit_unary_call` to take short names and reused them
  in `lower_binary_op`/`lower_unary_op`.

- [x] **CR-5: `match` lowering is broken** `src/ir/control.rs`
  Fixed: reuses `lower_if_pattern` for each arm with `next_bb` as the failure
  target. Linear chain of pattern checks — correct if not optimal (decision
  trees are a future optimization). Respects `binding_is_value` for ref/value mode.

- [x] **CR-6: `for` loop lowering is non-functional** `src/ir/control.rs`
  Fixed: implemented index-based iteration using `core::len` and `core::lt`.
  Handles both single and pair bindings. Respects `binding_is_value`.
  Later updated for new `for k, v in map` syntax and Sequence type.

- [x] **CR-7: Range lowering is non-functional** `src/ir/control.rs`
  Fixed: lowers to `core::make_seq(start, end, inclusive)` call producing
  a Sequence value. Note: `core::make_seq` builtin must be registered.

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
  - `..rest`: produces a zero-copy Sequence via `core::array_seq(arr, start, end, mutable)`.
    `SeqState::ArraySlice` has a `mutable` flag controlled by binding mode:
    `let` → immutable (by-value iteration), `with` → mutable (write-back to source).
  - `after` patterns: indexes from end using `len - after.len() + i`
  Note: requires `core::array_seq` builtin to be registered.

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
  Fixed: added `BinaryOperator::builtin_name()`, `UnaryOperator::builtin_name()`,
  and `AssignmentOp::builtin_name()` methods on the AST enums. All three lowering
  sites (`expr.rs`, `constant.rs`, `stmt.rs`) now use the shared methods.

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
- [ ] Implement `MakeSeq`/`ArraySeq`/`SeqNext` intrinsic runtime in `exec_intrinsic`
- [ ] For-loop type dispatch (Match on iterable type for unknown types)
- [ ] For-loop sequence path (SeqNext-based loop for Sequence type)
- [ ] Host sequence support (`SeqState::Host` variant, defer trait design to embedder API)
- [ ] Module/import resolution system
- [ ] Standard library: `std.cbor` (encode/decode)
- [ ] Standard library: `std.time` (now, format)
- [ ] Standard library: `std.encoding` (hex, base64)
- [ ] Standard library: `std.parsing` (parse_int, etc.)

### P2 — Optimization Passes

#### Type-Specialized Compilation (Phase 2 of intrinsic refactor)

Operators are now `IntrinsicOp` (Phase 1 complete). Phase 2 uses type analysis
to specialize them at compile time, eliminating runtime type dispatch.

**Dependency chain:**
```
TypeAnalysis (DONE) → Coercion Insertion → StepKind Emission → Peephole → Closures
```

- [ ] **Thread TypeAnalysis to compilation** — pass the `TypeAnalysis` result
      (currently computed but discarded with `let _types = ...`) through to
      `compile_program` / `compile_function`. Each VarId gets a known TypeSet.

- [ ] **Add `Widen` intrinsic for explicit type coercion** — new `IntrinsicOp::Widen`
      with a target type. Only 3 numeric coercion edges: UInt→Int, UInt→Float,
      Int→Float. Coercion is currently implicit in the 10-way match inside each
      arithmetic op; making it explicit enables folding and hoisting.

- [ ] **Coercion insertion pass** — new IR pass after type refinement. Walks
      `Intrinsic { op: Add, args: [v1, v2] }`, checks refined types of v1/v2.
      If mixed (e.g. UInt+Int), inserts `Intrinsic { op: Widen }` to promote
      the narrower operand, then rewrites the Add to operate on same-type args.
      If both already same type, no change. If unknown, leave for generic dispatch.
      Pipeline position:
      ```
      ... → Type Refinement → Coercion Insertion → Cleanup Const Fold → CFG Simplify
      ```

- [ ] **Define `StepKind` intermediate** — tagged enum between IR compilation
      and closure generation. Enables pattern-matching on instruction sequences.
      ```rust
      enum StepKind {
          // Type-specialized (when TypeAnalysis proves both args same type)
          AddUU { dest, a, b },    // u64 + u64, single checked_add
          AddII { dest, a, b },    // i64 + i64
          AddFF { dest, a, b },    // f64 + f64
          AddGeneric { dest, a, b }, // unknown types, full dispatch
          // Same pattern for Sub, Mul, Div, Mod, Lt, Eq, ...
          // Shared coercion steps (3 total, reused across all ops)
          WidenUIntToInt { dest, src },
          WidenUIntToFloat { dest, src },
          WidenIntToFloat { dest, src },
          // Non-arithmetic (always same codegen)
          Copy { dest, src },
          Const { dest, value },
          Call { dest, func, args },
          CallUser { dest, func_idx, args },
          Jump(usize),
          Branch { cond, then_, else_ },
          // ...
      }
      ```

- [ ] **Type-aware StepKind emission** — when compiling `Intrinsic { op: Add }`,
      consult TypeAnalysis for the operand types:
      - `(UInt, UInt)` → `StepKind::AddUU`
      - `(Int, Int)` → `StepKind::AddII` (after coercion pass widened operands)
      - unknown → `StepKind::AddGeneric`
      The specialized variants compile to a single `checked_add` with no enum
      match on Value — the type is statically known.

- [ ] **Peephole optimization on StepKind** — pattern-match on StepKind sequences
      before converting to closures:
      - Copy-to-self elimination: `Copy { dest: 5, src: 5 }` → remove
      - Dead store removal: write to slot never read → remove
      - Const+use fusion: `Const { dest: 3, value: 1 }` + `AddUU { a: 3, ... }`
        → `AddUU_Imm { a, imm: 1 }` (or just inline the constant)
      - Redundant coercion elimination: same Widen applied twice → reuse first
      - Coercion chain collapsing: UInt→Int then Int→Float → UInt→Float
      - Jump threading: `Jump(A)` where A is `Jump(B)` → `Jump(B)`

- [ ] **Loop-invariant coercion hoisting** — a Widen of a loop-invariant value
      can be moved to the pre-header. Common case: `for i in 0..n` where `n`
      is Int but `i` is UInt — the coercion of `n` only needs to happen once.

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

- [ ] **Tail-Call Optimization (TCO)** — rewrite tail calls to parameter overwrite
      + jump to entry. The flat pc-based executor supports this naturally.

- [ ] **Function Inlining** — clone callee IR into call site for small pure functions.

- [ ] **Sparse Conditional Constant Propagation (SCCP)** — combines constant
      propagation with unreachable code detection. Would subsume const_fold + guard_elim.

#### Diagnostics

- [ ] Dead-store warnings for non-ref-backed loop variable mutations
- [ ] Unused variable warnings (from DCE liveness data)

### P2 — Quality

- [ ] Integration test suite
- [ ] Fuzz testing for parser
- [ ] Documentation: API docs, embedding guide

### P3 — Future
- [ ] Compiled bytecode serialization format
- [ ] REPL / CLI tool
- [ ] LSP support
- [ ] Domain-specific module examples (DTN/BPSec)
- [ ] Loop unrolling (small loops with known iteration count)
- [ ] Escape analysis (stack-allocate non-escaping collections)
- [ ] Global Value Numbering (GVN) — more powerful CSE across blocks

## File Map

```
src/
  lib.rs              — Public API: compile(), Program::call(), re-exports
  compile.rs          — Link phase, closure compilation, phi elimination, flat pc executor
  ast.rs              — AST node types, Span, Spanned
  types.rs            — BaseType, TypeSet
  parser.rs           — Chumsky-based parser -> AST
  builtins.rs         — BuiltinRegistry for host-provided extern functions (empty by default)
  diagnostics.rs      — Error/warning accumulator with codes
  exec.rs             — VM, Heap, HeapVal, Value, Slot, Float
  ir/
    mod.rs            — Lowerer state, scope management, public lower() API
    types.rs          — IR types: VarId, BlockId, Instruction, Terminator, Program, etc.
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
      type_refinement.rs — Type set refinement
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
