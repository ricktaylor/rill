# Rill TODO

## Project Overview

Rill is a memory-safe, embeddable scripting language written in Rust. Architecture: Parser (chumsky) -> AST -> IR (SSA) -> VM (stack-based with heap tracking). ~13,400 lines of Rust across 21 source files.

## Current Status (per README + code inspection)

### Complete
- Grammar specification (ABNF) — `docs/grammar.abnf`
- Full parser with tests — `src/parser.rs` (2148 lines)
- AST and type definitions — `src/ast.rs`, `src/types.rs`
- VM core with stack/heap tracking — `src/exec.rs` (710 lines)
- Heap tracking system (CoW HeapVal, refcounted, limit-checked)
- Builtin registry — `src/builtins.rs` (1456 lines)
- Diagnostics system — `src/diagnostics.rs` (723 lines)
- IR type definitions — `src/ir/types.rs`
- IR lowering (AST -> IR) — `src/ir/` modules: program, stmt, expr, control, pattern, constant, const_eval
- Optimizer passes started — `src/ir/opt/`: const_fold, type_refinement, guard_elim, definedness

### In Progress
- [ ] IR lowering completeness — verify all AST nodes are handled
- [ ] Standard library modules — only prelude builtins registered so far

### Recently Completed
- [x] Sequence as internal type (`BaseType::Sequence`, `Value::Sequence`, `SeqState`)
- [x] New for-loop syntax: `for k, v in map` replaces `for [k, v] in map`
- [x] Pair binding: `for i, x in arr` gives index + element
- [x] Updated grammar.abnf, parser, AST, IR lowering, example docs

### Not Yet Started
- [ ] IR interpreter / code generator — no execution bridge from IR to VM
- [ ] Register `core::seq_next` builtin (advance sequence, return next or undefined)
- [ ] Register `core::make_seq` builtin (create sequence from start/end/inclusive)
- [ ] Register `core::array_seq` builtin (zero-copy array slice as Sequence)
- [ ] Register `core::collect` builtin (materialize sequence to array)
- [ ] For-loop type dispatch: emit Match on iterable type for unknown types
- [ ] For-loop sequence path: seq_next-based loop for Sequence type
- [ ] Dead-store warnings for mutations to non-ref-backed loop variables
- [ ] Host sequence support: `SeqState::Host` variant (defer trait vs callback decision to embedder API design)
- [ ] CBOR encode/decode integration
- [ ] Compiled bytecode format
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

## Task Backlog

### P0 — Critical Path (needed for end-to-end execution)
- [ ] Bridge IR to VM execution (IR interpreter or bytecode emission)
- [ ] Verify all expression types lower correctly (binary ops, unary ops, calls, indexing, field access, bit access `@`)
- [ ] Verify all statement types lower correctly (let, with, assign, augmented assign, return, for, while, loop, break, continue)
- [ ] Verify match/pattern lowering completeness (type patterns, destructuring, guards, rest patterns)
- [ ] End-to-end test: parse -> lower -> optimize -> execute

### P1 — Core Functionality
- [ ] Module/import resolution system
- [ ] Standard library: `std.cbor` (encode/decode)
- [ ] Standard library: `std.time` (now, format)
- [ ] Standard library: `std.encoding` (hex, base64)
- [ ] Standard library: `std.parsing` (parse_int, etc.)
- [ ] Error reporting with source spans through full pipeline
- [ ] Public API surface (`src/lib.rs` currently only declares modules, no re-exports)

### P2 — Optimization & Quality
- [ ] Dead code elimination pass
- [ ] Additional const folding cases
- [ ] Type narrowing through control flow
- [ ] Integration test suite
- [ ] Fuzz testing for parser
- [ ] Documentation: API docs, embedding guide

### P3 — Future
- [ ] Compiled bytecode serialization format
- [ ] REPL / CLI tool
- [ ] LSP support
- [ ] Domain-specific module examples (DTN/BPSec)

## File Map

```
src/
  lib.rs              — Crate root (module declarations only)
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
