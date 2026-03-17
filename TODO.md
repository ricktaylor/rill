# Rill TODO

## Project Overview

Rill is a memory-safe, embeddable scripting language written in Rust.
Architecture: Source → Parser (chumsky) → AST → Lower → IR (SSA) → Optimize → Compile (closure-threaded) → Execute (flat pc-based loop).

## Current Status (per README + code inspection)

### Complete
- Grammar specification (ABNF) — `docs/grammar.abnf`
- Full parser with implicit return support — `src/parser.rs`
- AST and type definitions — `src/ast.rs`, `src/types.rs` (TypeSet as u16 bitfield)
- VM core with stack/heap tracking — `src/exec.rs`
- Heap tracking system (CoW HeapVal, capacity-based, limit-checked)
- Builtin registry — `src/builtins.rs`
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
- [ ] Register sequence builtins: `core::make_seq`, `core::seq_next`, `core::array_seq`, `core::collect`
- [ ] For-loop type dispatch (Match on iterable type for unknown types)
- [ ] For-loop sequence path (seq_next-based loop for Sequence type)
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
- [ ] Register missing builtins: `core::make_seq`, `core::seq_next`, `core::array_seq`, `core::collect`
- [ ] For-loop type dispatch (Match on iterable type for unknown types)
- [ ] For-loop sequence path (seq_next-based loop for Sequence type)
- [ ] Host sequence support (`SeqState::Host` variant, defer trait design to embedder API)
- [ ] Module/import resolution system
- [ ] Standard library: `std.cbor` (encode/decode)
- [ ] Standard library: `std.time` (now, format)
- [ ] Standard library: `std.encoding` (hex, base64)
- [ ] Standard library: `std.parsing` (parse_int, etc.)

### P2 — Optimization Passes

#### IR-Level (SSA)

- [ ] **Type-Driven Dead Arm Elimination** — use the existing `TypeAnalysis` result
      (currently computed but discarded with `let _types = ...`) to prune Match arms
      where `TypeSet ∩ arm_type = ∅`. A Match with one surviving arm becomes a Jump.
      This feeds into CFG simplification → DCE. ~30 lines. Dependency chain:
      ```
      Type Analysis (DONE) → Dead Arm Elimination → CFG Simplify (DONE) → DCE
      ```

- [ ] **Dead Code Elimination (DCE)** — remove instructions whose dest VarId is
      never used. Iterate until stable (removing one may make operands dead).
      Respect purity: keep impure Calls even if result unused. ~50-80 lines.
      Consumes dead arms/blocks from type-driven elimination above.

- [ ] **Copy Propagation** — if `x = Copy(y)`, replace all uses of `x` with `y`
      and remove the Copy. Straightforward in SSA.

- [ ] **Common Subexpression Elimination (CSE)** — if the same pure operation
      with the same operands appears twice, reuse the first result. Requires
      purity checking (already have via `BuiltinMeta.purity`).

- [ ] **Algebraic Simplification** — simplify identities:
      `x + 0 → x`, `x * 1 → x`, `x * 0 → 0`, `x - x → 0`,
      `!!x → x`, `x && true → x`, `x || false → x`.
      Can be part of const folding or a separate pass.

- [ ] **Loop-Invariant Code Motion (LICM)** — lift pure computations whose
      operands are all defined outside the loop to the pre-header block.
      Requires: loop detection (back-edges), dominator tree, invariant analysis.

- [ ] **Tail-Call Optimization (TCO)** — detect calls in tail position, rewrite
      to parameter overwrite + jump to entry block. Eliminates frame allocation
      for recursive functions. The flat pc-based executor supports this naturally.

- [ ] **Function Inlining** — replace calls to small pure functions with the
      function body. Clone callee IR into call site. Valuable for helper functions.

- [ ] **Sparse Conditional Constant Propagation (SCCP)** — more powerful than
      current const fold. Combines constant propagation with unreachable code
      detection in a single pass. Would subsume const_fold + guard_elim.

#### Closure Compiler Level

- [ ] **Peephole Optimization** — between phi resolution and flattening.
      Requires tagged `StepKind` intermediate form before conversion to closures.
      Copy-to-self elimination, dead store removal, const+use fusion, jump threading.

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
  builtins.rs         — BuiltinRegistry, core builtin definitions
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
    const_eval.rs     — Compile-time constant evaluation
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
