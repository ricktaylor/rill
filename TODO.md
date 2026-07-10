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
- **Optimizer** — unified type-informed fixpoint loop (`src/opt/`)
  - All passes in single loop: const fold → CSE → copy prop → DCE → ref elision → coercion elision → CFG simplify → type analysis → coercion insertion → cast elision → algebra → condition fold → dead arm elim, repeating until convergence
  - CFG simplify includes jump threading (with Phi conflict detection) and Phi simplification (single-source, all-same-source, duplicate dedup)
  - Expression-level type guards for all intrinsics (len, collect, append, cast, compound assignment, for-loop Len, range), with guard cache for deduplication
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
    - [x] `ExternRegistry` — namespaces only (no globals), `register(def)` takes self-describing `ExternDef`
    - [x] `ExternDef` carries `namespace` + `name` — `ExternDef::new("math", "sqrt", impl)`
    - [x] `register()` returns `Result<(), RegistryError>`, `RegistryError::DuplicateInNamespace`
    - [x] `has_namespace()`, `get_in()`, `namespace_iter()`, `lookup()`
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
  - Phase 2a: Source file imports — **done**
    - [x] `SourceLoader` trait: `load(identifier, from) -> SourceResult { source, namespace, canonical_id }`
    - [x] `FileLoader` (filesystem) and `MemoryLoader` (in-memory) implementations
    - [x] `Compiler` builder: `new(&loader)`, `add_extern()`, `add()`, `add_source()`, `build()`
    - [x] Single loader per Compiler — canonical_id consistency guaranteed
    - [x] BFS import queue with canonical_id deduplication (no cycles possible)
    - [x] Import resolution: derive namespace from loader's `SourceResult.namespace`
    - [x] Import resolution: `as name` explicit alias, call-site rewriting
    - [x] Imported functions prefixed with canonical namespace in merged IR
    - [x] Diagnostic: "source file not found" (via SourceLoader::load error)
    - [x] Diagnostic: duplicate namespace alias (import vs import)
    - [x] Source tracking: `source_id` on `Diagnostic`, `SourceMap` on `Diagnostics`
    - [x] `parse()` accepts `source_id` parameter, stored in `AstProgram`
  - Phase 2b: Remaining module features
    - [x] `import "x" as _` (root merge) — two-pass parse/lower, `merged_imports` in lowerer
    - [x] Multi-file diagnostic rendering with `file:line:col`, source line, caret underline
    - [ ] Linker builder (bytecode + prelude template pattern)
    - [x] Import vs require namespace clash detection — error with `as` hint
  - Phase 3: Name clash enforcement — **done**
    - [x] Duplicate function name in same file → error (with note pointing to first definition)
    - [x] Function/constant name vs intrinsic (`len`, `collect`) → error
    - [x] Function/constant name vs global extern → error
    - [x] Function/constant name vs merged extern (`as _`) → error
    - [x] Constant name vs function name → error (with note)
    - [x] Global extern name vs intrinsic → `RegistryError` at registration time
    - [x] Shared `check_name_clash()` helper for consistent error messages
  - Phase 4: Visibility and DCE — **done** (2026-06-17)
    - [x] Track function origin: root file (public) vs imported file (private) —
          via a merge-time `root_names` set in `merge_ir` (the one place root-ness
          is structurally known), not a `Function` field that would ripple through
          the IR for data needed once
    - [x] DCE: imported functions not reachable from root → eliminate —
          `prune_dead_imports` (BFS over `Call` `qualified_name()` edges) runs at
          the end of `merge_ir`, before optimization, so dead functions are never
          optimized/monomorphized/compiled
    - [x] Root file functions always kept (potential embedder entry points) —
          roots are seeded by file origin, never in-degree
    - [x] Unused import warning — `W010_UnusedImport`, root file only, emitted at
          the import statement's span (added `path`/`span` to per-source
          `ImportEntry`). Predicate: no surviving `ns::*` function. Resolved the
          stale `used_functions` TODO in `compile/mod.rs` (whole-program DCE now
          lives at merge time)
- [x] **Expression spans** — `type Expr = Spanned<Expression>`. Parser wraps every expression
      node with its source span. `lower_expression` sets `current_span` from the expression
      span, giving precise error locations for undefined variables, type mismatches, etc.
- [x] **Optional `let` initializer** — `let x;` (no initializer) binds the
      pattern to Undefined. Consistent with Rill's SQL NULL semantics:
      Undefined is a real value, not an error. `with` still requires an
      initializer (a reference needs a target). AST `VarDecl.initializer`
      is `Option<Expr>`; the lowerer emits `emit_undefined()` when absent.
- [~] **File-scope variables (globals)** — `let x = expr;` or `let x;` at file scope.
      See `docs/globals_design.md` for full design. **Single-file done** (2026-06-16);
      multi-file + global-collection field write-back + link-time type narrowing deferred.
  - [x] Parser: allow `let` at top level, optional initializer
  - [x] Parser: `::name` syntax for global access in function bodies
  - [x] Parser: reject `let _ = expr;` and pattern destructuring at file scope
        (`global()` uses `ident()`; `_` rejected in lowerer's `collect_globals`)
  - [x] Lowerer: two-pass scan — `collect_globals` assigns slots, then `lower_init_function`
  - [x] Lowerer: `::name` resolves to `LoadGlobal`/`StoreGlobal`, bare name never resolves to global
        (except inside `__init__`, gated by `in_global_init`)
  - [x] IR: `LoadGlobal { dest, slot }` and `StoreGlobal { slot, value }` instructions
  - [x] VM: globals in first N stack slots (persistent across calls), frames allocated above
        (absolute `vm.get`/`vm.set` for globals; bp-relative `local`/`set_local` for frames)
  - [x] VM lifecycle — **user's model, not the doc's**: `vm.exec(&program)` resets + runs
        `__init__`; `VM` derives `Clone` (`vm.clone()` for an independent worker — no
        separate `fork()`); `VM::new()` stays public.
        (design doc's `Linker::exec(self)->VM` / private-`new` not adopted — see doc note)
  - [x] Compiler: synthetic `__init__` function, evaluates initializers in source order
  - [ ] Linker: chain init functions in import order (dependencies first) — DEFERRED (multi-file)
  - [x] Mutable: functions can reassign globals via `::name = value;` (incl. compound `+=`)
  - [x] Private to source file — imported-file globals rejected with a clear error (multi-file deferred)
  - [x] Type analysis: `any()` at compile time (link-time narrowing DEFERRED)
  - [x] Purity analysis: functions accessing globals marked impure (`collect_pure_functions`)
  - [ ] DEFERRED: in-place write-back to a global collection field/element
        (`::config.timeout = x`; `with` on a global binds the value, no write-back)
  - [ ] DEFERRED: multi-file globals (per-file slot offsetting + `__init__` chaining;
        merge pipeline is BFS-order, needs topological sort)
- [x] **Remove `const` keyword** — done (2026-06-16). Replaced by file-scope `let`.
      `const` declaration machinery deleted (parser/AST/`ir/constant.rs`); shared
      `const_eval.rs` + `ConstValue` kept (optimizer uses them). New Phase G pass
      `inline_const_globals` (`src/opt/const_globals.rs`) inlines never-written
      foldable globals to constants and drops `__init__`/slots when none remain.
      Globals read during init (chained/forward refs) stay runtime globals to
      preserve source-order Undefined semantics. Migration: `const NAME` →
      `let NAME` + `::NAME` at use sites; file-scope `let` takes no patterns.
- [ ] **Standard prelude** — Not a language feature. A conventional `prelude.rill`
      source file (is_defined, is_uint, default, etc.) that embedders provide
      via the SourceLoader. Scripts import it with `import "prelude.rill" as _;`.
- [x] **By-ref parameter passing** — Unified caller-emitted model:
      - Default is by-value (CoW clone — Rc increment). `with` opts into by-reference.
      - Caller emits `MakeRef` for by-ref args, plain value for by-val, at the call site.
      - Compiler is ref-agnostic: uniform `copy_slot_from` for all args.
      - TailCall uses `set_local` (writes through Refs) + `reset_local` (clears locals).
      - Reload instructions provide SSA visibility after by-ref calls.
      - Consistent across function params, for loops, and match arms.
- [ ] **Host objects** — `Value::Host(Box<dyn HostObject>)` with unified trait for
      embedder-provided data structures. Combines host sequences and generic accessors:
  - `HostObject` trait: `get(key)`, `set(key, value)`, `iter()`, `len()`
  - Accessor works through trait for Host values (`with x = host[key]`)
  - Sequence iteration works through trait (`for item in host { }`)
  - `len()` intrinsic dispatches through trait
  - Native Array/Map remain fast paths — trait dispatch only for Host values
  - Compiler specialisation: type analysis can prove base is Array/Map and skip
      trait dispatch, emitting direct closures (same pattern as arithmetic specialisation)
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
      `ssa/promote.rs` converts to SSA. Old ad-hoc phi insertion removed.
  - Phase 1: Pre-SSA IR — **done**
    - [x] `Instruction::Assign { slot, value }` and `Instruction::Read { slot, dest }`
    - [x] Lowerer emits Assign/Read instead of managing VarIds and scopes
    - [x] Old manual phi insertion removed
  - Phase 2: mem2reg pass — **done** (`src/ssa/promote.rs`)
    - [x] Cytron et al. (1991): IDF-based phi placement + dominator-tree renaming
    - [x] Cooper-Harvey-Kennedy (2001) dominator tree (`src/ssa/domtree.rs`)
    - [x] `DominatorTree` reusable analysis: `idom()`, `dominates()`, `dominance_frontier()`, `children()`
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
- [x] **Dominator tree + Cytron SSA** — replaced Braun et al. (2013) with
      Cytron et al. (1991) SSA construction. Cooper-Harvey-Kennedy (2001)
      dominator tree in `src/ssa/domtree.rs`, IDF-based phi placement +
      dominator-tree renaming in `src/ssa/promote.rs`. The tree is a reusable
      analysis (`DominatorTree::build()`) available for future passes:
      1. LICM (lift loop-invariant code to pre-header)
      2. GVN (more powerful CSE across blocks)
      3. Array bounds checking (recognise `i < len(arr)` guards dominating
         `arr[i]` and mark Index results as defined)
      Note: for-loop element bindings currently use explicit narrowing copies
      (`emit_copy(raw, TypeSet::defined())` in `lower_for_idx`, `emit_narrowing`
      in `lower_for_seq`) because the lowerer knows `i < len` holds. With
      dominator-based bounds checking, the type analysis would prove this
      automatically and these manual annotations can be removed.
- [ ] **SSA construction — code-review follow-ups** — remaining cleanups from
      the post-migration review of `src/ssa/`. (Correctness/robustness items are
      already fixed and committed: iterative dominator-tree renaming, no stack
      overflow on deep CFGs; deterministic phi placement and operand order;
      fixpoint trivial-phi elimination; O(1) precomputed dom-tree children;
      single-clone phi-resolution copies.) Still open:
  - [ ] **Pruned SSA placement** — `place_phis` inserts a phi at the IDF of
        *every* assigned slot regardless of liveness, leaving dead phis (with
        fresh undefined operands) for DCE to clear. Intersect IDF placement with
        per-block liveness (semi-pruned SSA) so the invariant lives in one pass.
        Shares a liveness analysis with the **Slot allocator** and **Dead
        write-back elimination** items below.
  - [ ] **Restore `test_shadowing_different_slots` assertion** — it was weakened
        during the migration to only reject a phi sourcing `VarId(0)` (it now
        tolerates the dead phi over an undefined operand). Restore a meaningful
        check, ideally once pruned SSA removes the dead phi.
  - [x] **Extract a shared CFG helper** — `ir::cfg::{block_map, reachable_blocks}`
        now back the reachability/lookup in `ssa/domtree.rs` and
        `opt/cfg_simplify.rs` (the latter also drops its O(n)-per-step
        `blocks.iter().find()` BFS). A new `Terminator` shape only has to be
        taught to `Terminator::successors()`. (The reachable+RPO-ordered+deduped
        predecessor map in `domtree` and the all-blocks one in
        `cfg_simplify::merge_block_chains` have different requirements and are
        left separate.)
  - [x] **Reuse `block_map`** — `DominatorTree::build` now takes a pre-built
        block map; `promote` builds it once via `cfg::block_map` and shares it
        with both the tree and renaming, so it is no longer built twice.
  - [x] **Drop redundant `processed` set** — in `place_phis`' IDF worklist,
        `idf` and `processed` were always mutated together; now `idf` doubles as
        the visited set.
  - [x] **Drop unused parameter** — `PromoteCtx::new` no longer takes the unused
        `_tree`.
  - [x] **Dedup duplicate successors (latent)** — `DominatorTree::build` now
        dedups a block's successors before recording predecessors, so a
        terminator naming the same target twice (`If` with `then == else`, or a
        `Match` sharing a block) yields one predecessor entry, not two.
        Covered by `test_duplicate_successor_dedup`.
- [x] **Liveness analysis** — done (2026-06-17). `src/ssa/liveness.rs`: backward
      dataflow over SSA mirroring `DominatorTree` (`Liveness::build(function,
      block_map)`, `live_in`/`live_out`/`used`/`is_used`), phi-aware (operands
      credited to the predecessor's live-out), deterministic (`BTreeSet`). The
      reusable keystone for the consumers below. `live_out` is consumed by the
      slot allocator; `live_in`/`used`/`is_used` stay gated
      `#[cfg_attr(not(test), allow(dead_code))]` (reserved, exercised by unit
      tests) like domtree's `idom`/`dominates`. Per-instruction operand shape
      consolidated into `src/ir/uses.rs` (was duplicated in dce/tail_call).
- [ ] **Dead write-back elimination** — a WriteRef exists but the base value is never
      read after the write-back point. Requires liveness analysis (now available).
- [ ] **Reload-generation alias decay + Rc churn** — the write-back discipline
      (Reload + reassign after every mutation) migrates the live value to a fresh
      SSA def, so a slot-resident Accessor created earlier keeps aliasing the
      pre-reload slot: it observes writes only up to the next reload generation
      (`with x = arr[0]; arr[1] = 5; arr[0] = 99; x` → 1; likewise a direct write
      after a by-ref call is invisible to the element binding). Each reload also
      leaves a stale Rc in the dead slot, forcing one extra CoW split on the next
      write. Candidate fixes: slot-allocator coalescing of reload chains onto one
      physical slot (old aliases then stay valid), or re-emitting the accessor
      from the name at each read.
- [x] **Slot allocator** — done (2026-06-17). `src/ssa/slot_alloc.rs`:
      `SlotAlloc::build(function, block_map)` coalesces non-interfering VarIds
      onto shared physical slots via an interference graph (per-instruction
      liveness from `Liveness`) + greedy coloring. Consumed at compile time in
      `compile_function` (`alloc.slot(var)` replaced the identity `slot(var)=var.0`;
      `frame_size` from the allocation) — **not** an opt phase and **no IR
      renumbering**, so `analyze_types` keeps per-VarId type-specialization
      precision. ~4.75x frame reduction measured (38 SSA vars → 8 slots).
      - Params pre-colored to positional slots `0..param_count` (calling
        convention adopts args there); tail-call functions keep the param region
        exclusive and the reset loop covers `param_count..frame_size`.
      - **Pinning** (private slot): Ref/Accessor dest/base/key (the VM stores slot
        indices captured at creation) and **all phi dests** (phi-resolution copies
        land at predecessor ends and run on critical edges, so a phi value can be
        physically live beyond SSA liveness — e.g. a `break` value preloaded at a
        loop header). v2 could refine to pin only critical-edge phis (or split
        critical edges) to coalesce the ubiquitous guard-phis.
      - Move/copy coalescing deferred to v2 (must be type-aware: narrowing copies
        carry a tighter type). `Program::function_frame_size` exposes the result.

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
- [ ] **Forbid `let _ = expr;`** — redundant; `expr;` already discards the result.
      Rill has no `#[must_use]` warnings to silence. `_` remains valid inside
      patterns (`let [_, b] = arr;`, `match x { _ => ... }`) but not as a
      standalone `let` binding. Emit a compile error with hint: "use `expr;` instead".
- [ ] **Revisit destructuring `let` validity** — decide whether `let [x, y] = ..;`
      (array/rest destructuring in a value binding) should be valid syntax at all,
      or whether destructuring belongs only in conditional contexts (`if let`,
      `match`) where match-failure has a branch. A non-conditional `let [x,y] = e;`
      silently binds Undefined on shape mismatch (duck-typing) — possibly a footgun.

### P2 — Diagnostics

- [ ] Dead-store warnings for non-ref-backed loop variable mutations — deferred.
      A sound version needs per-slot reaching-definitions (the SSA use-set/liveness
      doesn't express per-store deadness, and the loop-accumulator case is
      false-positive-prone); not bundled with the unused-variable lint.
- [x] Unused variable warnings — done (2026-06-17). `W001_UnusedVariable`, emitted
      by `check_unused_bindings` (`src/ir/lint.rs`) on the **pre-SSA** function
      (user names don't survive SSA), before `promote`. Flags body value bindings
      (`let`/`for`/pattern/match) never read; excludes params, `with`/ref bindings,
      and `_`. Points at the binding pattern; suggests an `_` prefix.

### P2 — Quality

- [ ] Integration test suite
- [ ] Fuzz testing for parser
- [ ] Documentation: API docs, embedding guide

### P2 — Portability

- [ ] **`no_std` + `alloc` support** — the core library has no real `std` dependency
      beyond import paths. All runtime types use `alloc` primitives (`Vec`, `String`,
      `Rc`, `Box`). Dedicated pass to enable `#![no_std]` with `extern crate alloc`:
  - [ ] `std` feature flag (enabled by default) — gates `FileLoader` and any future I/O
  - [ ] Replace `std::collections::HashMap` with `hashbrown` (or re-export via feature)
  - [ ] Switch all imports to `alloc::`/`core::` paths (`alloc::vec::Vec`, `alloc::rc::Rc`,
        `alloc::string::String`, `core::fmt`, `core::hash`, etc.)
  - [ ] `MemoryLoader` and `SourceLoader` trait remain available without `std`
  - [ ] `FileLoader` behind `#[cfg(feature = "std")]`
  - [ ] Verify `indexmap` `no_std` support (has it, needs `hashbrown` backend)
  - [ ] Verify `chumsky` `no_std` compatibility (may need feature flag or parser behind `std`)
  - [ ] WASM target smoke test

### P3 — Future

- [ ] **StepKind peephole layer** — tagged enum between IR compilation and closure
      generation. Enables multi-instruction fusion (counter increment, accumulator
      update, compare+branch, index+guard, seq advance+guard). Only fuse when
      type-specialized variants exist. See design notes below.
- [ ] **`rill` script runner** (`tools/rill/`) — shebang-compatible interpreter.
      See `docs/cli_design.md` for full design.
  - [ ] `tools/rill/` crate with stdlib externs (io, process, str, math, encoding, fmt)
  - [ ] `fn main(args)` entry point convention, exit code from return value
  - [ ] Standard prelude auto-imported (`prelude.rill` via `include_str!`)
  - [ ] `rill script.rill [args]` — no subcommands (shebang-compatible)
  - [ ] `rill` with no args → runs built-in REPL (written in Rill, using stdlib externs)
- [ ] **`rillc` compiler toolchain** — separate binary with subcommands.
  - [ ] `rillc check` — parse + optimize, report diagnostics
  - [ ] `rillc dump` — dump optimized IR
  - [ ] `rillc build` — compile to bytecode
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

*Every loop iteration (highest impact):*

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Counter increment | `i = i + 1` | `Const(1)` + `AddUU(i,c)` + `Copy` | `IncUU { slot, imm: 1 }` | 3→1 |
| Accumulator update | `sum = sum + x` | `AddUU(sum,x)` + `Copy` | `AddAssignUU { dest, src }` | 2→1 |
| Loop condition | `i < len` → branch | `LtUU(i,len)` + `BranchIf` | `BranchLtUU { a, b, t, f }` | 2→1 |
| Array element read | `arr[i]` with guard | `Index` + `Guard` | `IndexGuard { dest, base, key, fail }` | 2→1 |
| Seq advance | `SeqNext` + guard | `SeqNext` + `Guard` | `SeqNextGuard { dest, seq, fail }` | 2→1 |

*Most functions (moderate impact):*

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Const + binop | `x + 5` | `Const(5)` + `AddUU(x,c)` | `AddImmUU { dest, src, imm: 5 }` | 2→1 |
| Compare + branch | `if x == 0` | `EqUU(x,c)` + `BranchIf` | `BranchEqUU { a, b, t, f }` | 2→1 |
| Negate + branch | `if !cond` | `Not(c)` + `BranchIf` | `BranchIf` with swapped targets | 2→1 |
| Copy-to-self | SSA artifact | `Copy(x, x)` | eliminated | 1→0 |

*Write-back paths (ref-backed mutations):*

| Pattern | Source | Steps | Fused | Savings |
|---------|--------|-------|-------|---------|
| Compute + write-back | `x += 1` (ref) | `AddUU` + `WriteRef` + `Copy` | `AddWriteRefUU { ... }` | 3→1 |
| Const array literal | `[1, 2, 3]` | `Const` × 3 + `MakeArray` | `MakeArrayConst { values }` | 4→1 |

*Destructure-collect patterns (sub-slicing):*

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
Unified fixpoint (repeat until convergence):
  Const Fold → CSE → Copy Prop → DCE → Ref Elision → Coercion Elision
    → CFG Simplify (jump threading, Phi simplification)
    → Type Analysis → Coercion Insertion → Cast Elision → Algebraic Simplification
    → Non-Bool Condition Folding → Dead Match Arm Elimination

Diagnostics (post-convergence):
  W009 type mismatch, W201 definedness warnings (TypeSet-based)

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
    domtree.rs        — Dominator tree (Cooper-Harvey-Kennedy 2001)
    promote.rs        — Cytron et al. (1991) SSA construction (IDF phi placement + dom-tree renaming)
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
