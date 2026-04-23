# Module System Design (Phase 2)

## Status

Phase 1 (extern namespaces + `require`) is done. This document covers
Phase 2: source file imports.

## Source Tracking

Spans remain `SimpleSpan<usize>` (chumsky 0.12's `&str` input hardcodes
this type — custom context requires wrapping the input as a `Stream`).

Instead, source identity is tracked alongside spans:

- `Diagnostic.source_id: Option<Rc<str>>` — which file the error is in
- `Diagnostics.source_map: SourceMap` — maps canonical_id → source text
- `AstProgram.source_id: Rc<str>` — set by `parse()`, used for diagnostics
- `parse()` takes `source_id: &str` parameter

This gives multi-file diagnostics without touching every span in the AST.
A future chumsky version with native span context support could embed
`Rc<str>` directly in spans.

## Loader Traits

### SourceLoader (used by Compiler)

```rust
pub struct SourceResult {
    pub source: String,        // UTF-8 source text
    pub namespace: String,     // default namespace for this module
    pub canonical_id: String,  // unique identity for deduplication
}

pub trait SourceLoader {
    /// Load source text from an import identifier.
    /// `from` is the canonical_id of the importing file (None for root).
    /// The loader resolves relative paths, returns canonical identity.
    fn load(&self, identifier: &str, from: Option<&str>) -> Result<SourceResult, String>;
}
```

The loader resolves paths and returns a canonical ID. The compiler
deduplicates on `canonical_id` — two different relative paths resolving
to the same file are loaded once.

### LibraryLoader (used by Linker)

```rust
pub trait LibraryLoader {
    /// Load pre-compiled bytecode.
    fn load(&self, identifier: &str) -> Result<Vec<u8>, String>;
}
```

### Provided implementations

- `FileLoader` — filesystem, resolves relative paths, canonical = absolute path
- `MemoryLoader` — in-memory map, canonical = key string
- Embedders can provide custom loaders (database, network, etc.)

## Public API

### Traits

```rust
pub struct SourceResult {
    pub source: String,        // UTF-8 source text
    pub namespace: String,     // default namespace for this module
    pub canonical_id: String,  // unique identity for deduplication
}

pub trait SourceLoader {
    /// Load source text. `from` is the canonical_id of the importing file (None for root).
    fn load(&self, identifier: &str, from: Option<&str>) -> Result<SourceResult, String>;
}
```

### ExternDef

All externs are namespaced. `ExternDef` is self-describing — carries
namespace, name, and implementation:

```rust
ExternDef::new("math", "sqrt", sqrt_impl)
    .param("x", TypeSet::numeric())
    .returns(TypeSet::numeric())
    .pure_infallible()
```

Scripts access externs via `require`:
```rill
require math;           // math::sqrt() available qualified
require math as _;      // sqrt() available unqualified
```

### Compiler (implemented)

The Compiler takes a single SourceLoader at construction — guarantees
canonical_id consistency across all files.

```rust
let loader = FileLoader::new("./scripts");
let mut compiler = Compiler::new(&loader);

// Register externs (namespace is part of the ExternDef)
compiler.add_extern(ExternDef::new("math", "sqrt", sqrt_impl))?;

// Add source (imports resolved recursively via the loader)
compiler.add("main.rill");

// Build → Program with all functions compiled
let (program, warnings) = compiler.build()?;

// Execute
let mut vm = VM::new();
let result = program.call(&mut vm, "main", 0)?;
```

### Linker (planned — Phase 2b)

For bytecode serialization and the prelude template pattern:

```rust
let mut linker = Linker::new(&loader);
linker.add_extern(ExternDef::new("math", "sqrt", sqrt_impl))?;
linker.add_bytecode(&lib_loader, "prelude.rillc")?;  // pre-compiled
linker.add("main.rill");                               // compile on the fly
let (program, warnings) = linker.build()?;
```

### Entry points (planned — Phase 4)

Entry points would declare which functions are embedder-callable and
their expected param types. Drives DCE (unreachable functions eliminated)
and type propagation (param types seed interprocedural analysis).

```rust
// Future API:
let (program, warnings) = compiler.build_entry("main", &[TypeSet::uint()])?;
```

No entries declared → all functions kept, params default to `any()`.
This is the current behavior.

## Import Resolution

### Syntax (already parsed)

```rill
import "utils.rill";              // namespace = "utils" (filename stem)
import "utils.rill" as helpers;   // namespace = "helpers"
import "utils.rill" as _;         // merge into root scope
```

### Resolution Steps

1. Lowerer encounters `import` in `lower_program`
2. Calls `loader.load(path)` → gets source text
3. Parses the imported source with `SourceId = Rc::from(path)`
4. Recursively lowers the imported `AstProgram`
5. Functions/constants added to the caller's scope under the namespace

### Import Resolution (BFS Queue)

No recursive resolution, no cycle detection needed. Imports are
processed as a breadth-first queue:

1. Parse root file → collect `import` statements → add to queue
2. Load + parse each queued file → collect more imports → add to queue
3. Deduplicate by `canonical_id` (already queued → skip)
4. Continue until queue is empty
5. All IR fragments accumulated in the compiler

"A imports B, B imports A" just means both are in the queue — each is
loaded once, the linker resolves cross-references. No recursion, no
stack, no cycles.

```
Queue processing:
  main.rill → imports "utils", "proto"
  queue: [utils, proto]
  
  utils.rill → imports "proto", "../common"
  proto already queued (skip), common added
  queue: [proto, common]
  
  proto.rill → imports "../common"
  common already queued (same canonical_id, skip)
  queue: []  → done
```

Bytecode has no `import` statements — all imports were resolved at
`save()` time. Loading bytecode just deserialises IR fragments.

### Design Decisions

- **Import alias conflicts** — `import "a" as utils; import "b" as utils` is an
  error. Use `as` to disambiguate. Same rule for import vs require conflicts.
- **Span context** — `Rc<str>` carries the `canonical_id` from the loader. Ties
  spans to unique file identity for diagnostics.
- **Error recovery** — loader error for one import doesn't stop compilation.
  Continue processing remaining imports, accumulate diagnostics.
- **Import position** — top-level only (alongside `require`, `const`, `fn`).
  No imports inside function bodies.
- **Re-imports** — A and B both import "common". Loaded once (deduplicated by
  canonical_id). Each file gets its own namespace binding independently.

### Namespace Resolution

Same mechanism as `require`:
- Default: functions available as `namespace::func()`
- `as alias`: available as `alias::func()`
- `as _`: merged into root scope (name clashes detected)

The lowerer has `require_aliases` and `merged_externs` maps for externs,
and `merged_imports` for `import ... as _`. Call resolution uses the same
namespace mechanism — the only difference is the source.

### Unqualified Name Resolution Order

When resolving an unqualified call `foo()`:

1. **Intrinsics** — `len`, `collect`, `append` (compiler built-ins)
2. **Local user functions** — defined in the same source file
3. **Merged imports** — from `import "file" as _`
4. **Externs** — global (registered without namespace) and merged (`require ns as _`)

First match wins. Local functions and imports shadow externs — a warning
is emitted. The original is always reachable via qualified `ns::func()`
syntax.

### Visibility

- Imported functions are **visible to the importing file only**
- An imported file's own imports are **NOT re-exported**
- The root file's functions are always public (embedder entry points)

This means: if A imports B, and B imports C, A cannot call C's functions.
Each file's imports are private.

Implementation: track which functions came from which import. During
lowering, the scope only includes the current file's imports + root scope.

## Diagnostics

### Multi-file Errors

With `Rc<str>` in spans, diagnostics know which file each span belongs to.
Error rendering:

```
error[E100]: undefined variable `foo`
  --> utils.rill:12:5
   |
12 |     foo + 1
   |     ^^^ not found in this scope
```

The filename comes from `span.context` (the `Rc<str>`).

### New Error Codes

- `E501_CircularImport` — "circular import: A imports B imports A"
- `E502_ImportNotFound` — "source file not found: utils.rill"
- `E503_DuplicateNamespace` — "namespace 'utils' already defined (import vs require)"

### Source Text for Caret Display

Diagnostics need the source text to show the line with the caret. With
multi-file, the diagnostics system needs a map from filename → source text.
This can be:
- Passed alongside compilation results
- Stored in a `SourceMap` that accumulates during compilation
- Threaded through the `Diagnostics` struct itself

## Standard Prelude (embedder convention, not a language feature)

There is no built-in prelude. Utility functions like `is_defined()`,
`is_uint()`, `default()` are ordinary Rill source that the embedder
can provide via the import system:

```rill
// prelude.rill — written once, imported by scripts that need it
fn is_defined(x) { match x { _ => { true } } }
fn is_uint(x) { match x { UInt _ => { true }, _ => { false } } }
fn default(x, fallback) { if is_defined(x) { x } else { fallback } }
```

```rill
// user script
import "prelude.rill" as _;
fn process(data) { default(data.value, 0) }
```

No special compiler support needed — this is just regular imports.
The embedder controls what's available by configuring the SourceLoader.

## Future: Incremental Compilation + Deferred Link

Use case: compile multiple sources independently, link once.

Both Compiler and Linker accumulate `IrProgram` fragments and extern
registrations. `build()` merges them, runs interprocedural optimisation
(monomorphisation, return type inference), resolves all `Call` references,
and compiles to closures.

`IrProgram` fragments and extern declarations are data — cloneable.
Closures are only created at `build()` time. The template pattern
(clone a Linker after adding the prelude) works without any `Arc`
gymnastics.

## Implementation Order

### Done (Phase 2a)

1. Source tracking: `source_id` on `Diagnostic` + `SourceMap` on `Diagnostics`
   (chumsky 0.12 `&str` input hardcodes `SimpleSpan<usize>` — Rc context deferred)
2. `parse()` accepts `source_id` parameter, stored in `AstProgram`
3. `SourceLoader` trait + `FileLoader` + `MemoryLoader` implementations
4. `Compiler` builder: `new(&loader)`, `add_extern()`, `add()`, `add_source()`, `build()`
   - Single loader per Compiler — guarantees canonical_id consistency
   - BFS import queue with canonical_id deduplication
   - Call-site rewriting: import alias → canonical namespace
   - Imported functions prefixed with canonical namespace in merged IR
5. Duplicate namespace alias detection, import-not-found errors

6. `import "x" as _` (root merge) — two-pass parse/lower, `merged_imports` in lowerer

### Remaining (Phase 2b)

7. Multi-file diagnostic rendering with `file:line:col` format
8. Linker builder (bytecode + prelude template pattern)
9. Entry-point-driven DCE

Note: per-file visibility enforcement is not needed — the grammar only allows
single-level `namespace::func()`, so there's no syntax to reach through
another file's imports.
