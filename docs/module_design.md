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

// Execute (exec initializes any file-scope globals; no-op otherwise)
let mut vm = VM::new();
vm.exec(&program)?;
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

### Two-Pass Import Resolution

Imports are resolved in two passes, not recursively:

**Pass 1: `parse_source_tree()`** — BFS load and parse all files.
Uses a `VecDeque` for breadth-first traversal. Each file is parsed
with `ast::parser::parse()`, collecting ASTs and import metadata
(aliases, canonical IDs). Deduplicates by `canonical_id` — two
different relative paths resolving to the same file are loaded once.
Detects namespace clashes (duplicate aliases, import vs require
conflicts) during this pass.

**Pass 2: `lower_parsed_sources()`** — Lower each parsed file with
its `merged_imports` populated from `as _` imports. Builds a
`file_functions` index from all ASTs, then for each file constructs
a `merged_imports: HashMap<Identifier, Identifier>` mapping function
names to their canonical namespace. Calls `ir::lower_with_imports()`
with this map so the lowerer can resolve unqualified calls to
`as _`-imported functions.

```
Pass 1 (BFS parse):
  main.rill → imports "utils", "proto"
  queue: [utils, proto]
  
  utils.rill → imports "proto", "../common"
  proto already queued (skip), common added
  queue: [proto, common]
  
  proto.rill → imports "../common"
  common already queued (same canonical_id, skip)
  queue: []  → all parsed

Pass 2 (lower each file):
  For each parsed file, build merged_imports from its `as _` imports,
  then lower with the merged_imports map.
```

Finally, `merge_ir()` combines all lowered IR programs into a single
`IrProgram`, rewriting call instructions to use canonical namespaces
(e.g., `helpers::foo()` → `utils::foo()` if the file was loaded as
`utils` by the SourceLoader).

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
- **Visibility enforcement** — The two-pass design provides structural
  enforcement: each file's `merged_imports` is built independently from its
  own import declarations, so transitive imports are never visible.

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

The two-pass design enforces this structurally: each file's
`merged_imports` is built from its own import declarations only.

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

### Error Codes

Module-related errors use existing diagnostic codes:

- `E400_DuplicateDefinition` — duplicate import namespace alias, import
  vs require namespace clash, or duplicate function name across files
- `E501_MissingEntryPoint` — required entry point function not found
- `E502_CyclicDependency` — cyclic dependency detected during import resolution

### Source Text for Caret Display

Diagnostics need the source text to show the line with the caret. The
`Diagnostics` struct contains a `SourceMap` that accumulates source text
during compilation, keyed by `canonical_id`. The `render()` method looks
up the relevant source text via the diagnostic's `source_id` field.

## Standard Prelude (embedder convention, not a language feature)

There is no built-in prelude. Utility functions like `is_defined()`,
`is_uint()`, `default()` are ordinary Rill source that the embedder
can provide via the import system:

```rill
// prelude.rill — written once, imported by scripts that need it
fn is_defined(x) { if let _ = x { true } else { false } }
fn is_uint(x) { match x { UInt(_) => true, _ => false } }
fn default(value, fallback) { if let v = value { v } else { fallback } }
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
   - Two-pass architecture: `parse_source_tree()` + `lower_parsed_sources()`
   - Call-site rewriting: import alias → canonical namespace
   - Imported functions prefixed with canonical namespace in merged IR
5. Duplicate namespace alias detection, import-not-found errors
6. `import "x" as _` (root merge) — two-pass parse/lower, `merged_imports` in lowerer
7. Multi-file diagnostic rendering: `render()`, `format_location()`, `render_all()`
   with `file:line:col` format and source context with caret display

### Remaining (Phase 2b)

8. Linker builder (bytecode + prelude template pattern)
9. Entry-point-driven DCE

Note: per-file visibility enforcement is not needed — the grammar only allows
single-level `namespace::func()`, so there's no syntax to reach through
another file's imports. The two-pass design provides the structural guarantee:
each file's `merged_imports` is built independently from its own import
declarations.
