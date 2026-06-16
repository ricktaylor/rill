# File-Scope Variables (Globals) — Design

## Motivation

Rill has two kinds of file-scope declarations:
- `let NAME [= expression];` — file-scope variables (globals)
- `fn name(...) { }` — functions

File-scope `let` replaced the former `const` keyword, unifying all variable
bindings under one keyword. The optimizer determines which globals are
effectively constant (never written, foldable initializer, not read during
init) and inlines them automatically (`src/opt/const_globals.rs`). This avoids
a class of bugs where compile-time const-evaluation of `Purity::Const` externs
produced stale values when bytecode is loaded with different externs from those
present at compile time — non-foldable globals now initialize at load time with
the actually-linked externs.

Use cases for file-scope `let`:
- **Memoization/singletons**: precomputed lookup tables, parsed config, cached results
- **Module state**: private mutable state shared across functions in a file without passing it as arguments
- **Accumulators**: counters, collected results, running totals across function calls

## Design

### Syntax

```rill
let MAX = 100;                        // optimizer inlines (never written, literal init)
let threshold = compute_default();    // evaluated once at load time
let lookup = build_table(256);        // evaluated once at load time
let best_result;                      // starts as Undefined — assigned later

fn process(x) {
    if x > ::threshold { ... }        // :: required for globals in function bodies
}
```

### `const` Removal (done)

The `const` keyword has been removed; file-scope `let` replaces it. The
`inline_const_globals` pass (Phase G, `src/opt/const_globals.rs`) detects
never-written foldable globals and inlines them:

| Pattern | Optimizer behavior |
|---|---|
| `let MAX = 100;` (never written, not read in init) | Inlined to a literal; slot eliminated, `__init__` dropped if it was the only global |
| `let count = 0;` (written by a function) | True mutable global, kept in its slot |
| `let DOUBLE = MAX * 2;` (reads another global in its init) | Left as a runtime global, computed once at load time |

A global **read during `__init__`** (a chained or forward reference) is *not*
inlined: forward references read `Undefined` by source-order semantics, so
folding the constant in would change the result. Such globals stay runtime
globals — correct, with a one-time load cost.

`Purity::Const` externs remain useful for function-body const-folding
(`sqrt(4.0)` → `2.0`), but file-scope initialization always runs at load time
with the actually-linked externs. This prevents stale values in pre-compiled
bytecode.

### Semantics

- **Optional initializer**: `let x = expr;` or `let x;` (starts as Undefined)
- **Mutable**: functions in the same file can reassign globals
- **Private to source file**: not visible to importing files
- **Evaluation order**: initializers run top-to-bottom during `vm.exec()`
- **No `with` at file scope**: `with` creates a reference to another location —
  meaningless at file scope where there is no enclosing variable to reference.
  `with` inside function bodies can still bind to globals for structured mutation.

### Global Access — `::` Prefix

Inside function bodies, globals are accessed with the `::` prefix. A bare
name never resolves to a global — it's local scope, then error.

```rill
let count = 0;

fn increment() {
    ::count = ::count + 1;    // explicit global read + write
}

fn process() {
    let count = 99;           // local — no ambiguity with global
    ::count = count;          // global = local, crystal clear
}

fn broken() {
    count = count + 1;        // ERROR: 'count' not declared (no local)
}
```

At file scope (in global initializers), bare names resolve to other globals —
no `::` needed since there are no local scopes to conflict with.

**Name resolution inside functions:**
1. Local scope stack (block lets, loop vars, match bindings, params)
2. `::name` — file-scope globals (`LoadGlobal`)
3. Error

**File-scope name clashes (all errors):**
- Global vs global (duplicate name)
- Global vs function (same name)
- Global vs intrinsic (`len`, `collect`, `append`)

### Initialization — exec/fork/call

**Unix process model** (as implemented — `exec` is a method *on the VM* that
loads a program's globals into it, not a constructor):

| Operation | Unix | Rill |
|---|---|---|
| Load program | `exec()` | `VM::exec(&mut self, &Program) -> Result<()>` |
| Copy process | `fork()` | `vm.clone()` |
| Run function | system call | `Program::call(&self, &mut vm, name, argc)` |

- **`VM::exec(&mut self, program: &Program)`** — resets the VM, reserves the
  program's global slots (0..N), and runs `__init__` (evaluates all global
  initializers in source order), leaving the VM ready. Call once after
  `compile()`. A cheap reset for global-free programs.
- **`vm.clone()`** — deep copy of the VM state (the child inherits initialized
  globals; CoW values share heap accounting). `VM` derives `Clone`; there is no
  separate `fork()` — `clone()` already expresses the copy.
- **`Program::call(&self, &mut vm, name, argc)`** — run a function. No special
  first-call behavior — the VM is ready after `exec()`.

`VM` derives `Clone` and `VM::new()` stays public (lowest-churn). This differs
from an earlier sketch where `exec` was a linker method returning the VM and
`new` was private; the method-on-VM form was chosen so `exec` reads naturally
(`vm.exec(program)` loads) without forcing every existing `VM::new()` call site
to change.

```rust
let (program, _diags) = rill::compile(source, &externs)?;

let mut vm = VM::new();
vm.exec(&program)?;                    // __init__ runs, globals ready

program.call(&mut vm, "setup", 0)?;    // just a function call

let mut worker = vm.clone();           // independent, pre-initialized
worker.push(batch)?;
program.call(&mut worker, "process", 1)?;
```

### Forward References

A global's *initializer* may only reference globals declared **earlier** in the
file. The root scope is a scope: referencing a global before its declaration is a
use-before-definition error, exactly as it is inside a function. (Slots are still
allocated in a first pass so that *function bodies* may reference any global via
`::name` regardless of order — functions run after `__init__`.)

```rill
let a = 10;
let b = a + 1;    // OK — a is declared earlier; b = 11

let b = a + 1;    // ERROR: undefined variable `a` (declared later)
let a = 10;

let a = a + 1;    // ERROR: undefined variable `a` (not in scope during its own init)
```

A global declared without an initializer is in scope (holds Undefined) for later
initializers — that is the explicit meaning of `let g;`:

```rill
let g;            // Undefined
let h = g;        // OK — g is declared earlier; h = Undefined
```

No dependency analysis or reordering is done — initialization is strictly source
order, and out-of-order references are rejected rather than silently read as
Undefined.

### Mutability

Functions in the same file can reassign globals:

```rill
let counter = 0;

fn increment() {
    ::counter = ::counter + 1;
}

fn get_count() { ::counter }
```

**Purity implications**: any function that reads or writes a global is
impure. The interprocedural purity analysis tracks global access:
- A function that reads a global cannot be reordered past a call that writes it
- A function that writes a global cannot be eliminated by DCE
- The optimizer marks functions as impure if they access globals

**`with` binding to globals:**

```rill
let config = { timeout: 30, retries: 3 };

fn update_timeout(t) {
    with timeout = ::config.timeout {
        timeout = t;    // writes back to config.timeout
    }
}
```

### Restrictions at File Scope

File-scope `let` supports only simple named bindings:

```rill
let x = compute();         // OK: simple binding
let y;                     // OK: starts as Undefined
let [a, b] = compute();    // ERROR: pattern not allowed at file scope
let _ = do_startup();      // ERROR: discard not allowed at file scope
```

**No patterns**: destructuring at file scope creates multiple globals from
one expression, with complex failure modes.

**No discard**: `let _ = expr;` at file scope smuggles imperative side-effect
code into declarations. A global exists to be read; if nothing reads it, it
shouldn't be a global.

### Type Analysis

**Mental model**: a global is a zero-argument function returning a mutable
reference to a persistent VM slot. The existing type analysis, purity
tracking, and monomorphization infrastructure already reasons about function
arguments and returns — globals extend this naturally:

- **LoadGlobal** = calling a zero-arg function that returns the slot's value
- **StoreGlobal** = write-back through a mutable reference
- **Never-written global** = pure zero-arg function returning a constant —
  the const folder eliminates it

**At compile time (intraprocedural)**: the global's type is `any()`.

**At link time (interprocedural Phase B)**: type analysis narrows from:
- **Initializer type**: `let x = 42` → `{UInt}`, `let x = compute()` →
  inferred from return type, `let x;` → `{Undefined}`
- **Write analysis**: union of all `StoreGlobal` value types across the
  program

This feeds into monomorphization and inlining alongside function parameter
and return type inference.

## Storage Model

### VM Representation

Globals occupy the first N slots of the VM stack, persistent across function
calls. Function frames are allocated above the global region.

```
VM Stack:
+---------+---------+-----+---------+---------+-----+
| glob 0  | glob 1  | ... | param0  | local0  | ... |
+---------+---------+-----+---------+---------+-----+
  0         1               N         N+1
  <-- persistent -->       <-- function frame -->
```

Function `call()` preserves slots 0..N and allocates the frame above.
Function `ret()` truncates back to the frame base but never below N.

### IR Representation

Two new instructions:

```rust
LoadGlobal { dest: VarId, slot: u32 }    // read global into local SSA var
StoreGlobal { slot: u32, value: VarId }  // write to global slot
```

Each `LoadGlobal` produces a fresh VarId — the global's value may change
between loads due to intervening function calls.

### Initialization Function

The compiler generates a synthetic `__init__` function for each source file
that has globals. This function evaluates each global initializer in source
order and stores results via `StoreGlobal`. `vm.exec(&program)` runs `__init__`
to leave the VM initialized.

**Multi-file programs:** init functions run in import order (dependencies
first) during `exec()`.

### Compiled Representation

```rust
struct CompiledProgram {
    functions: Vec<CompiledFunction>,
    func_index: HashMap<String, usize>,
    global_count: usize,               // number of global slots
    init_func: Option<usize>,          // index of __init__ function
    warnings: Diagnostics,
}
```

## Multi-File Globals

When file A imports file B:
1. B's globals are part of B's compiled output
2. B's `__init__` runs before A's during `vm.exec()`
3. B's globals are in B's slot range; A's are in A's slot range
4. A cannot reference B's globals (private) — only B's functions

Global slot indices are assigned per-file during lowering, then offset
during linking to produce unique indices across the merged program.

## Implementation Dependencies

- **Slot allocator** (P2): globals add `LoadGlobal`/`StoreGlobal` VarIds.
  The slot allocator runs as Phase S after all optimization and before
  closure compilation. Globals occupy slots 0..N; function locals start
  at N+. Consider implementing alongside globals.

## Future Extensions

- **Lazy initialization**: `lazy let x = expensive();` — evaluated on
  first access rather than at load time.
- **Global type declarations**: `let x: UInt = compute();` — explicit
  type annotation for documentation and narrower type analysis.
