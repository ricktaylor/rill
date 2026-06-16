# Rill Language Design Document

This document captures the design of the Rill language.

## Overview

Rill is a memory-safe, semi-compiled embeddable scripting language. It compiles
to closure-threaded code with type-specialized arithmetic — no interpreter loop,
no bytecode decode overhead. The type system uses practical duck-typed scalars
and collections (booleans, integers, floats, text, bytes, arrays, maps) —
similar to what you'd find in Python, Lua, or JSON — making it natural for
processing structured data without schema declarations.

**Core features:**

- **Semi-compiled execution**: Source → SSA IR → optimized closures. No bytecode interpreter.
- **Type-specialized arithmetic**: Static analysis narrows types; the compiler emits direct
  `u64::checked_add` instead of runtime type dispatch when types are provably known.
- **Duck-typed values**: Nine user-visible base types covering scalars (Bool, UInt, Int, Float),
  strings (Text, Bytes), and collections (Array, Map, Sequence), plus an internal Undefined
  type used by the compiler for type analysis. No type annotations in source code — types
  are inferred by the optimizer.
- **Pattern matching**: Rich destructuring with type narrowing and reference binding.
- **Safe embedding**: Resource limits (stack, heap), no undefined behavior, host-provided externs.
- **Undefined propagation**: Failed operations produce undefined values that propagate silently —
  no exceptions, no panics. Scripts can probe data structures without defensive checks.

**Use cases:**

- Embedded scripting for applications (configuration, policy, rules)
- Structured data validation and transformation
- Data pipeline processing (filter, transform, enrich)
- Domain-specific scripting (network protocols, IoT, document processing)

## Architecture

```
Source Code
    │
    ▼
┌─────────┐
│ Parser  │  (chumsky) → AST
└────┬────┘
     │
     ▼
┌─────────┐
│   IR    │  SSA form, type sets, externs
└────┬────┘
     │
     ▼
┌─────────┐
│   VM    │  Stack-based execution with heap tracking
└─────────┘
```

## Terminology: Core and Externs

Rill has three distinct categories of functionality. These terms are used
consistently throughout the codebase and documentation.

### Core (Intrinsics)

Operations required by the language runtime to function. These are the
`IntrinsicOp` variants — `Add`, `Eq`, `MakeArray`, `MakeMap`, `Len`,
`SeqNext`, etc. The compiler knows their exact semantics, arity, types,
and const-eval behavior.

Core operations are **not user-callable by name** (with the exception of
`len()` and `collect()` which have syntactic shortcuts, and `append()` which
has a dedicated `Instruction::Append`). They exist only as lowering targets
for syntax. `x + y` lowers to `Intrinsic(Add, [x, y])`.
They are encoded as `IntrinsicOp` discriminants in bytecode and are always
available — no registry, no linking, no import.

### Prelude (convention, not a feature)

There is no built-in prelude. Utility functions like `is_defined()`,
`is_uint()`, and `default()` are ordinary Rill source that the embedder
provides via the import system:

```rill
// prelude.rill — provided by the embedder via the SourceLoader
fn is_defined(x) { if let _ = x { true } else { false } }
fn is_uint(x) { match x { UInt(_) => true, _ => false } }
fn default(value, fallback) { if let v = value { v } else { fallback } }
```

Scripts that need these functions import them explicitly:

```rill
import "prelude.rill" as _;    // merge into root scope
```

No special compiler support — this is the regular import mechanism.
The embedder controls what's available by configuring the SourceLoader.

### Externs (ExternRegistry)

Rust functions registered by the embedder. This is the embedding API.
All externs are namespaced — grouped under a namespace by the embedder.
Scripts use `require` to declare dependencies:

```rust
// Embedder (Rust side):
registry.register(ExternDef::new("runtime", "exit", exit_impl))?;
registry.register(ExternDef::new("cbor", "decode", decode_impl))?;
registry.register(ExternDef::new("console", "log", log_impl))?;
```

```rill
// Script (Rill side):
require cbor;           // cbor::decode() available qualified
require console;        // console::log() available qualified
require runtime as _;   // exit() available unqualified

cbor::decode(bytes)
console::log("hello")
exit(0)                 // unqualified via `as _`
```

`ExternDef` is self-describing — carries namespace, name, metadata
(parameter types, return type, purity), and implementation. Registered
via `ExternRegistry::register(def)`. In IR, externs appear as
`FunctionRef { namespace, name }` — symbolic references resolved at
compile/link time.

### Summary

| Category | Implementation | Callable by name? | In bytecode as | Available without registry? |
|----------|---------------|-------------------|----------------|---------------------------|
| **Core** | `IntrinsicOp` (Rust enum) | No (lowering targets) | `Instruction::Intrinsic` | Yes — always |
| **Externs** | Rust (embedder-provided) | Yes (`ns::func()` or unqualified via `as _`) | `FunctionRef { namespace, name }` | No — needs `ExternRegistry` + `require` |

The key architectural boundary is between **resolved** (core intrinsics +
user/imported source — compiled into the program) and **late-bound**
(externs — resolved against `ExternRegistry` at link time). The compiled
program contains resolved closures; only extern symbols require the host.

## Files

| File / Directory | Purpose |
|------------------|---------|
| `docs/grammar.abnf` | Formal grammar specification |
| `src/types.rs` | Core type definitions (BaseType, TypeSet, NumericType, ConvertMode, SliceMode) |
| `src/ast/types.rs` | Abstract syntax tree types |
| `src/ast/parser.rs` | Chumsky-based parser → AST |
| `src/ast/mod.rs` | AST module root (re-exports) |
| `src/ir/mod.rs` | IR types: Instruction, Terminator, MatchPattern, FunctionRef, Literal, BasicBlock |
| `src/ir/types.rs` | IR enums: IntrinsicOp, Terminator, MatchPattern, Instruction |
| `src/ir/expr.rs` | Expression lowering (AST → IR) |
| `src/ir/stmt.rs` | Statement lowering |
| `src/ir/control.rs` | Control flow lowering (if/while/for/loop/match) |
| `src/ir/pattern.rs` | Pattern lowering |
| `src/ir/program.rs` | Program-level lowering (functions, imports) |
| `src/ir/const_eval.rs` | Compile-time constant evaluation for intrinsics |
| `src/ir/constant.rs` | Constant binding handling |
| `src/ssa/promote.rs` | SSA promotion: Cytron phi placement + dominator-tree renaming |
| `src/ssa/domtree.rs` | Dominator tree (Cooper-Harvey-Kennedy 2001) |
| `src/opt/` | Optimization passes (see Optimization Pipeline) |
| `src/compile/mod.rs` | IR-to-closure compilation, CompiledProgram, Step, Action |
| `src/compile/exec.rs` | Instruction compilation to closures (intrinsics, calls, refs) |
| `src/compile/specialize.rs` | Type-specialized closure generation |
| `src/compile/terminator.rs` | Terminator compilation (If, Match, Jump, Return, TailCall) |
| `src/exec.rs` | Virtual machine, runtime values, stack, heap tracking |
| `src/externs.rs` | Extern function registry and metadata |
| `src/loader.rs` | SourceLoader trait, FileLoader, MemoryLoader |
| `src/diagnostics.rs` | Diagnostic codes, severity, source location tracking |
| `src/lib.rs` | Public API: compile(), Compiler, Program, FunctionHandle |

---

## Type System

### Runtime Types

| Type | Rust Representation | Description |
|------|---------------------|-------------|
| `Bool` | `bool` | Boolean |
| `UInt` | `u64` | Unsigned 64-bit integer |
| `Int` | `i64` | Signed 64-bit integer |
| `Float` | `Float` wrapper | 64-bit IEEE 754 (NaN excluded) |
| `Text` | `HeapVal<String>` | UTF-8 string |
| `Bytes` | `HeapVal<Vec<u8>>` | Byte string |
| `Array` | `HeapVal<Vec<Value>>` | Ordered collection |
| `Map` | `HeapVal<IndexMap<Value, Value>>` | Insertion-ordered key-value map |
| `Sequence` | `HeapVal<SeqState>` | Lazy single-pass iterator (internal) |

**Note on Sequence:** Sequence is an internal type for lazy, single-pass values
(e.g., `0..10` creates a Sequence, not an Array). It is not user-visible as a type
name — users cannot pattern match on it. They interact with sequences through `for`
loops and `collect()`. The `..` operator is described as creating "a sequence", not
"a range object."

### Undefined Values

Rill's undefined semantics are inspired by SQL's `NULL`: a missing or invalid
value that propagates silently through operations. Like `NULL`, undefined poisons
arithmetic (`undefined + 1` → `undefined`), comparisons (`undefined == undefined`
→ `undefined`, not `true`), and field access (`undefined.field` → `undefined`).
Scripts probe for presence with `if let` rather than catching exceptions.

- **Internal tracking**: `Value::Undefined` and `BaseType::Undefined` are explicit enum
  variants used internally by the VM and compiler. Users never name or pattern-match on
  `Undefined` directly — it is the implicit result of failed operations.
- **NaN → Undefined**: Float operations producing NaN return Undefined
- **Failed operations → Undefined**: Type errors, out of bounds, overflow, division by zero
- **Propagation**: Undefined propagates through operations; use `if let`/`if with` for handling

### Float Wrapper

```rust
pub struct Float(f64);  // Invariant: never NaN
```

- `Float::new(f64) -> Option<Self>`: Returns `None` for NaN
- Implements `Eq` and `Hash` via bit representation
- Enables `Value` to be used as map key

---

## Value Representation

### Scalars (Inline)

```rust
Value::Bool(bool)
Value::UInt(u64)
Value::Int(i64)
Value::Float(Float)
```

### Heap-Allocated (Tracked)

```rust
Value::Bytes(HeapVal<Vec<u8>>)
Value::Text(HeapVal<String>)
Value::Array(HeapVal<Vec<Value>>)
Value::Map(HeapVal<IndexMap<Value, Value>>)
Value::Sequence(HeapVal<SeqState>)
```

### Absence

```rust
Value::Undefined
```

### SeqState

Internal state for lazy sequences:

```rust
pub enum SeqState {
    Range { current: u64, end: u64 },
    ArraySlice { source: HeapVal<Vec<Value>>, start: usize, end: usize },
}
```

- **Range**: Created by `0..10` / `0..=10`. O(1) memory. Always unsigned, always
  exclusive — inclusive ranges are normalized at construction time by incrementing
  `end` via `saturating_add(1)`.
- **ArraySlice**: Created by `..rest` patterns. Zero-copy reference to source array.
  Mutability (by-value vs write-back) is handled at the IR level via the `SliceMode`
  parameter on the `ArraySeq` intrinsic — the runtime slice doesn't need to know.

### HeapVal<T>

Custom wrapper for heap tracking. Only 8 bytes (single Rc pointer):

```rust
struct Tracked<T> {
    heap: HeapRef,  // Rc<Heap> with Cell<usize>
    data: T,
}
pub struct HeapVal<T: HeapSize>(Rc<Tracked<T>>);  // 8 bytes
```

**Size optimization:** By embedding HeapRef inside the Rc'd allocation,
HeapVal is just one pointer. This keeps Value at 16 bytes and Slot at 16 bytes,
improving cache locality across the 65K-slot stack.

**Features:**

- **Accurate tracking**: Both allocations and deallocations tracked
- **CoW semantics**: `make_mut()` clones on write if shared
- **Cheap cloning**: Just bumps Rc refcount
- **Dynamic size**: `HeapSize` trait computes size on demand
- **Mutation tracking**: `update_heap_size(old, heap)` after size-changing ops

**Lifecycle example:**

```rust
// Function creates big array
let arr = vm.push_array(big_vec)?;  // heap.used += size

// Do work, fold to single value
let result = fold(arr);

// Function returns, frame truncated
// Array dropped, refcount → 0
// heap.used -= size  ← automatically reclaimed!
```

---

## Virtual Machine

### Stack Layout

```
┌────────┬────────┬─────┐       ┌──────────────────┐
│ param0 │ local0 │ ... │       │ FrameInfo(bp,ret) │
└────────┴────────┴─────┘       └──────────────────┘
  bp+0     bp+1                   frame_stack (separate)
```

- **Slot 0+**: Parameters, then locals (no frame slot in the value stack)
- **Separate frame stack**: FrameInfo stored in `VM.frame_stack: Vec<FrameInfo>`
- Frame info is not interleaved with values — cleaner slot addressing

### Slot Types

```rust
pub struct FrameInfo {
    pub bp: usize,
    pub return_slot: Option<usize>,
}

pub enum Slot {
    Val(Value),                        // Actual value (16 bytes)
    Ref(usize),                        // Near pointer: another stack slot
    Accessor { base: usize, key: usize }, // Far pointer: collection[key]
}
// Slot is 16 bytes total (Value is largest; Accessor fits in 2×usize = 16 bytes)
```

Two kinds of slot indirection (analogous to x86 near/far pointers):
- **Slot::Ref(target)** — near pointer: follows to another slot
- **Slot::Accessor { base, key }** — far pointer: base slot + key slot → element in collection

Collections (Array, Map) hold Values, not Slots. There is no shared heap —
elements are accessed through an Accessor, which is a (base, key) pair that
knows how to get/set a value in a collection. Reading through an Accessor
does `base_collection[key]`. Writing through an Accessor mutates the element.

Refs and Accessors compose: `Slot::Ref` pointing to a `Slot::Accessor` follows
the full chain on read and write. `vm.set_local` resolves through Ref→Accessor
automatically, enabling `with y = x; y = 10` where x is an Accessor.

### Call Convention

```rust
// Caller: evaluate args, create refs/accessors as needed
// For by-val params: pass value directly
// For by-ref params: emit MakeRef (creates Slot::Ref to caller's slot)

// Set up callee frame (frame info stored on separate frame_stack)
vm.call(frame_size, return_slot)?;

// Copy args via copy_slot_from (shallow: Ref stays Ref, Val stays Val)
for (i, &s) in arg_slots.iter().enumerate() {
    vm.copy_slot_from(i, caller_bp + s);  // args start at slot 0
}

// ... execute callee ...

// Callee returns value
vm.ret();
```

### Reference Binding (`with`)

Reference bindings create tracked aliases so that mutations flow back to the
source. Two IR instructions create bindings; the VM handles write-back
automatically through Slot resolution.

**Accessors** (`with x = arr[i]`):

```
// IR:
v0 = MakeAccessor { base: arr, key: i }     // creates Slot::Accessor
// bind "x" → v0

// x = 10  →  assignment to accessor-backed variable
v1 = Const(10)
WriteAccessor { base: arr, key: i, value: v1 }  // direct element write
Reload(arr)                                      // SSA visibility
```

At runtime, `MakeAccessor` creates `Slot::Accessor { base, key }` in the dest
slot. Reading `x` indexes into the collection. `WriteAccessor` writes directly
to the collection element (type-specialized at compile time). `Reload` creates
a fresh SSA def so the optimizer knows the collection was mutated.
`set_array_elem` or `set_map_entry` based on the collection type.

**Refs** (`with x = y`, or by-ref function params):

```
// IR:
v0 = MakeRef { base: y }                    // creates Slot::Ref
// bind "x" → v0

// x = 10
v1 = Const(10)
WriteRef { ref_var: v0, value: v1 }          // vm.set_local resolves Ref
// → writes to y's slot directly
```

At runtime, `MakeRef` creates `Slot::Ref` pointing to the base's slot (with
path compression — resolves existing Ref chains). Writing through a Ref
follows the chain to the target slot.

**Chaining** (`with x = arr[i]; with y = x; y = 10`):
- x = Slot::Accessor(arr, i)
- y = Slot::Ref(x's slot)
- `y = 10`: set_local follows Ref → finds Accessor → writes to arr[i]

The full chain resolves automatically through vm.set_local's Slot dispatch.

**For-loop accessors** (`for with x in arr { x += 1 }`):

```
// IR (body block):
v_acc = MakeAccessor { base: iter, key: i_phi }  // accessor to iter[i]
// bind "x" → v_acc

// x += 1  →  compound assignment
v_old = v_acc                                     // reads through Accessor
v_new = Intrinsic(Add, [v_old, 1])
WriteAccessor { base: iter, key: i_phi, value: v_new }  // direct element write
Reload(iter)                                             // SSA visibility
// rebind "x" → v_new
```

The write-back is emitted at the point of assignment — correct even with
`break` and `continue` (no deferred write-back needed). Note: for-loop
default is now by-value; `with` keyword opts into by-ref/accessor iteration.

**Ref origin tracking:** The lowerer maintains a scoped `ref_origins` map
(`Identifier → RefOrigin { ref_var, base, key }`) alongside the normal scope
stack. When a ref-backed variable is assigned, the lowerer looks up its
`RefOrigin` and emits `WriteRef`.

**Four IR instructions for references:**
- `MakeAccessor { dest, base, key }` → creates `Slot::Accessor` (far pointer into collection)
- `MakeRef { dest, base }` → creates `Slot::Ref` (near pointer to another slot)
- `WriteAccessor { base, key, value }` → direct element write, type-specialized at compile time
- `WriteRef { ref_var, value }` → write through a Ref/Accessor binding (VM resolves)

No `build_ref_map` tracing needed — each instruction is self-describing.
The compiler specializes WriteAccessor based on type analysis (Array vs Map).
WriteRef uses `vm.set_local` which resolves through Slot::Ref and Slot::Accessor.

### Resource Limits

| Resource | Limit | Error |
|----------|-------|-------|
| Stack | 65,536 slots (`DEFAULT_STACK_SIZE`) | `StackOverflow` |
| Heap | 16 MB (`DEFAULT_HEAP_LIMIT`) | `HeapOverflow` |

---

## Execution Model: Closure-Threaded Code

### Overview

Rill uses a **closure-threaded** execution model. The IR is compiled into Rust
closures at load time — each IR instruction becomes a closure that captures its
resolved operands. At runtime, there is no interpreter dispatch switch; closures
ARE the instructions.

```
Source → Parse → AST → Lower → IR → Optimize → Compile → Closures
                                                              │
                                                              ▼
                                                         Execute loop
```

### Why Closures?

| Approach | Dispatch cost | Operand cost | Portability |
|----------|--------------|--------------|-------------|
| Switch interpreter | Per-instruction match | Per-instruction decode | High |
| Bytecode VM | Per-instruction dispatch | Register/stack decode | High |
| Closure-threaded | None (closure IS the op) | None (captured at compile) | High |
| Machine code JIT | None | None | Low (arch-specific) |

Closures give near-JIT performance characteristics (no dispatch, no operand decode)
while remaining fully portable Rust. The Rust compiler can inline small closures.

### Compiled Representation

```rust
struct CompiledProgram {
    functions: Vec<CompiledFunction>,
    func_index: HashMap<String, usize>,  // name → index
    warnings: Diagnostics,               // link-phase warnings
}

struct CompiledFunction {
    steps: Vec<Step>,           // all closures, flattened contiguously
    block_starts: Vec<usize>,   // block i starts at steps[block_starts[i]]
    entry: usize,               // index into block_starts
    frame_size: usize,          // VM slots to reserve
    param_count: usize,         // number of parameters
}

type Step = Box<dyn Fn(&mut VM, &CompiledProgram) -> Result<Action, ExecError>>;

enum Action {
    Continue,                   // advance pc by 1
    NextBlock(usize),           // jump to block_starts[idx]
    Call { func_id, frame_size }, // inline user function call
    TailCall { func_id },       // self-recursive tail call (reuse frame)
    Return(Value),              // return from function (Undefined = void)
    Exit(Value),                // hard exit to driver
}
```

Every closure — instructions AND control flow — is a `Step`. There is no
separate terminator type. The last step of each block returns `NextBlock`,
`Return`, or `Exit` instead of `Continue`.

### Compilation Pipeline

```
IR blocks (SSA with phis)
    │
    ├─ 1. Compile each IR instruction to a Step closure
    │     (VarIds → slot offsets, externs → fn pointers)
    │
    ├─ 2. Compile each terminator to a Step closure
    │     (If/Match/TailCall → NextBlock closures)
    │
    ├─ 3. Resolve phis: insert Copy steps into predecessor blocks
    │     (eliminates ALL phi nodes — no runtime prev_block tracking)
    │
    ├─ 4. [Future] Peephole optimize each block
    │     (copy elimination, dead stores, const+use fusion)
    │
    └─ 5. Flatten all blocks into a single contiguous Vec<Step>
          with block_starts offsets
```

**Phi elimination** (step 3) works by moving the copy to each predecessor:

```
// Before (SSA phi in join block):
then_block: ..., Jump(join)
else_block: ..., Jump(join)
join: phi(dest=5, [(then, slot_3), (else, slot_7)])

// After (copies in predecessors, no phi):
then_block: ..., Copy(slot_5 <- slot_3), Jump(join)
else_block: ..., Copy(slot_5 <- slot_7), Jump(join)
join: // nothing — value already in slot_5
```

Identity phis (all sources are the same slot as dest) are dropped entirely.

### What Closures Capture

| IR concept | Compile-time resolution |
|------------|------------------------|
| `VarId(n)` | Stack slot offset `n` |
| `FunctionRef("cbor::decode")` | Native function pointer (via ExternRegistry) |
| `Literal::UInt(42)` | Pre-computed `Value` captured directly |
| `Literal::Text("key")` | Interned on first execution (Rc clone after) |
| `BlockId` | Index into `block_starts` |
| `IntrinsicOp` | Resolved at compile time — per-op closure, no runtime dispatch |

### Compile-Time Specialization

The compiler uses TypeAnalysis and DefinednessAnalysis to emit optimized
closures, eliminating runtime dispatches when static information is sufficient:

| Specialization | Condition | Effect |
|---|---|---|
| Scalar Const | Bool/UInt/Int/Float literal | Value pre-computed, zero runtime work |
| String/Bytes Const | Text/Bytes literal | Interned: allocates once, Rc clone after |
| Intrinsic op dispatch | Always | Per-op closure (no `match op` at runtime) |
| Binary arithmetic | Both args same single type | Direct typed operation (e.g. `u64::checked_add`) |
| Convert target | Target is compile-time parameter | Target resolved at compile time, source-only dispatch |
| Index/MakeRef | Base type known | Type-specific indexing (no 5-way dispatch) |
| WriteAccessor | Base type known | Direct `set_array_elem` or `set_map_entry` |
| Match (single-arm) | From `if let` patterns | Inlined type/literal/length test |
| Match (multi-arm) | From `match` expressions | Pre-compiled predicate closures |
| Copy | Source provably Defined | Direct `.unwrap()` (no None check) |
| If condition | Provably Bool + Defined | Direct bool read (no null/type check) |
| Intrinsic args | All args provably Defined | `.unwrap()` then call (skip Option gate) |
| Non-Bool condition | Optimizer folds to Jump | `debug_assert!` in compiler |
| Identity Convert | Optimizer elides to Copy | `debug_assert!` in compiler |

### Calling Convention

All function calls (user and extern) use frame-based argument passing — no
intermediate `Vec` allocation:

- **User calls**: caller copies args slot-to-slot into callee's frame, executes
  callee body inline (same loop, no `execute_function` indirection)
- **Extern calls**: frame set up with `call_with_args` (Lua-style: pre-pushed
  args adopted into frame, already in place at bp). Externs read args via `vm.arg(i)`
- **Entry point**: embedder pushes args with `vm.push()`, calls with `argc`

### Execution Loop

The executor is a single flat loop with a program counter:

```rust
let mut pc = func.block_starts[func.entry];
loop {
    match (func.steps[pc])(vm, program)? {
        Action::Continue    => pc += 1,
        Action::NextBlock(i) => pc = func.block_starts[i],
        Action::Return(val) => { vm.ret(); return Ok(val); }
        Action::Exit(val)   => { vm.ret(); return Ok(None); }
    }
}
```

**Key properties:**

- **One loop, one match**: No nested loops, no separate terminator dispatch.
  The branch predictor sees one site where ~95% of outcomes are `Continue`
  (`pc += 1`).
- **Contiguous step array**: All closures for a function are in a single `Vec`.
  Step pointers (fat pointers, 16 bytes each) are cache-friendly.
- **No phi overhead at runtime**: All phis resolved to copies in predecessors
  during compilation. No `prev_block` tracking.
- **No Rust stack growth for loops**: Back-edges set `pc` to an earlier offset.
- **User function calls inline**: Caller sets up frame and runs callee loop
  directly, bounded by VM's `MAX_STACK_SIZE` (65K slots, ~3000-6000 levels).
- **Zero allocation per call**: Args copied slot-to-slot, no Vec. Externs
  use frame-based `vm.arg(i)` access.
- **Linear blocks are merged**: CFG simplification (runs twice in optimizer)
  concatenates chains of single-predecessor/single-successor blocks. The
  closure compiler only emits `NextBlock` for genuine runtime branches.

### Future: Peephole Optimization

After phi resolution but before flattening, each block is a `Vec<Step>` that
can be inspected and optimized. This requires a tagged intermediate form:

```rust
enum StepKind {
    Copy { dest: usize, src: usize },
    Const { dest: usize, value: Value },
    Call { dest: usize, func: ExternFn, args: Vec<usize> },
    // ...
}
// Optimize StepKind sequences, then convert to closures
```

Candidates: copy-to-self elimination, dead store removal, constant + immediate
use fusion, jump threading.

### Tail-Call Optimization

When a function's last action is calling itself (self-recursive tail position),
the current frame is reused instead of pushing a new one. The TCO pass
(`src/opt/tail_call.rs`) detects `Call + Return` chains where the callee is
the enclosing function and rewrites them to `TailCall` terminators:

```
// Before TCO:
fn factorial(n, acc) {
    if n == 0 { return acc; }
    return factorial(n - 1, acc * n);  // pushes new VM frame
}

// After TCO (IR transform):
fn factorial(n, acc) {
    if n == 0 { return acc; }
    n = n - 1;          // rewrite params in current frame
    acc = acc * n;
    TailCall [n, acc];  // jump to entry, no new frame
}
```

The flat pc-based architecture supports this naturally — `TailCall` overwrites
params and sets `pc` to the entry offset instead of recursing through
`execute_function`. Currently self-recursive only — mutual tail calls are
not optimized.

---

## Intermediate Representation

### Design Philosophy

- **SSA form**: Single Static Assignment for optimization
- **Pattern lowering**: Complex patterns → primitive operations
- **Intrinsics minimal**: Only short-circuit operators (`&&`, `||`)
- **Explicit references**: `MakeRef`/`WriteRef` make ref semantics visible to the optimizer

### Two Categories of Operations

| Category | Description | Examples |
|----------|-------------|----------|
| **Core intrinsic** | Language-defined operations with fixed semantics | `Add`, `Eq`, `Len`, `MakeArray` |
| **Extern call** | Embedder-provided functions (via `ExternRegistry`) | `exit()`, `cbor::decode()`, `console::log()` |

**Core intrinsics** (`IntrinsicOp` enum) cover all language-defined operations:

- Arithmetic: `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Neg`
- Comparison: `Eq`, `Lt`
- Logical: `Not` (note: `&&`/`||` lower to control flow, not intrinsics)
- Bitwise: `BitAnd`, `BitOr`, `BitXor`, `BitNot`, `Shl`, `Shr`, `BitTest`, `BitSet`
- Collection: `Len`, `MakeArray`, `MakeMap`, `Collect`
- Sequence: `MakeSeq`, `ArraySeq(SliceMode)`, `SeqNext`
- Coercion: `Convert(NumericType, ConvertMode)`

Intrinsics emit `Instruction::Intrinsic { op, args }` in the IR. The compiler
knows their exact semantics, arity, result types, and fallibility — enabling
const folding, type refinement, and type mismatch diagnostics without any
registry lookup. `1 + 2` lowers to `Intrinsic(Add, [1, 2])` which the optimizer
folds to `3` using the inline const evaluator.

**Some intrinsics expand to control flow** instead of a single instruction:

- `is_uint(x)` → `Match(x, [(Type(UInt), BB_t)], BB_f)` + Phi → Bool
- `is_defined(x)` → `Match(x, [(all defined types, BB_t)], BB_f)` + Phi → Bool
- `x && y` → `If(x, evaluate_y, false)` + Phi (short-circuit)
- `x || y` → `If(x, true, evaluate_y)` + Phi (short-circuit)

**Extern calls** (`Instruction::Call`) are for embedder-provided functions
registered via the `ExternRegistry`. The standard registry is empty — all
language-defined operations are core intrinsics. `Call` is also used for
user-defined functions (including functions from imported source files),
which resolve internally within the program rather
than against the registry.

### Pattern Lowering Example

```
AST: let [a, b] = arr;

IR:
BB0:
    Match(arr, [(Array(2), BB_bind)], BB_fail)

BB_bind:
    %a = Index(arr, 0)
    %b = Index(arr, 1)
    Jump(BB_continue)

BB_fail:
    %a = Undefined
    %b = Undefined
    Jump(BB_continue)

BB_continue:
    // %a and %b are Phi nodes merging from BB_bind and BB_fail
```

Nested patterns are decomposed left-to-right:

```
AST: let [UInt(x), Text(s)] = arr;

IR:
BB0:
    Match(arr, [(Array(2), BB_elem0)], BB_fail)

BB_elem0:
    %e0 = Index(arr, 0)
    Match(%e0, [(Type(UInt), BB_elem1)], BB_fail)

BB_elem1:
    %x = Copy(%e0)
    %e1 = Index(arr, 1)
    Match(%e1, [(Type(Text), BB_success)], BB_fail)

BB_success:
    %s = Copy(%e1)
    Jump(BB_continue)

BB_fail:
    %x = Undefined
    %s = Undefined
    Jump(BB_continue)

BB_continue:
    // execution continues
```

**Reference pattern** — `with` bindings use `MakeAccessor` instead of `Index`,
enabling write-back via `WriteAccessor`:

```
AST: with [a, b] = arr;

IR:
BB0:
    Match(arr, [(Array(2), BB_bind)], BB_fail)

BB_bind:
    %k0 = Const(0)
    %a = MakeAccessor(arr, %k0)    // accessor to arr[0]
    %k1 = Const(1)
    %b = MakeAccessor(arr, %k1)    // accessor to arr[1]
    Jump(BB_continue)

BB_fail:
    %a = Undefined
    %b = Undefined
    Jump(BB_continue)

BB_continue:
    // a and b are accessor-backed: assignment emits WriteAccessor
    // e.g. a = 10  →  WriteAccessor(arr, %k0, 10) + Reload(arr)
```

### Intrinsic Operations

All language-defined operations are `IntrinsicOp` variants, compiled directly
without registry lookup. Each intrinsic carries metadata methods:

- `is_fallible()` — whether it can return undefined (overflow, type mismatch)
- `result_type()` — static result type (e.g. `Add` → `{UInt, Int, Float}`)
- `result_type_refined(arg_types)` — refined result using promotion lattice
- `param_type(index)` — required type per argument (for mismatch detection)

| Category | Syntax | IntrinsicOp | Fallible |
|----------|--------|-------------|----------|
| Arithmetic | `+` `-` `*` `/` `%` `-x` | `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Neg` | Yes (overflow) |
| Comparison | `==` `<` | `Eq`, `Lt` | No / Yes |
| Comparison | `!=` `>` `<=` `>=` | Expanded to `Eq`/`Lt`/`Not` | — |
| Logical | `!` | `Not` | No |
| Logical | `&&` `\|\|` | Control flow (`If` + Phi), not intrinsics | — |
| Bitwise | `&` `\|` `^` `~` `<<` `>>` | `BitAnd`, `BitOr`, `BitXor`, `BitNot`, `Shl`, `Shr` | No |
| Bit access | `@` | `BitTest` (read), `BitSet` (write) | Yes (OOB) |
| Collection | `len(x)` `[a,b]` `{k:v}` | `Len`, `MakeArray`, `MakeMap` | Yes / No / Yes |
| Collection | `collect(seq)` `append(arr,v)` | `Collect`, `Append` (via `Instruction::Append`) | No / No |
| Sequence | `start..end` `..rest` `seq_next` | `MakeSeq`, `ArraySeq(SliceMode)`, `SeqNext` | No |
| Coercion | (implicit promotion) | `Convert(target, Checked)` | Yes (overflow) |
| Cast | `x as UInt` | `Convert(target, Unchecked)` | No |

Short-circuit operators (`&&`, `||`) lower to control flow (If + Phi), not
intrinsics. `append()` lowers to `Instruction::Append` (a separate instruction,
not `Instruction::Intrinsic`, because it is side-effecting). All other operators
lower to `Instruction::Intrinsic { op, args }`.

Reflexive comparisons expand during lowering:
- `a != b` → `Not(Eq(a, b))`
- `a > b` → `Lt(b, a)` (swap)
- `a <= b` → `Not(Lt(b, a))`
- `a >= b` → `Not(Lt(a, b))`

**Type-refined result types:** The type refinement pass uses `result_type_refined()`
to narrow intrinsic results based on operand types. For example, `Add(UInt, UInt)`
produces `{UInt}` not `{UInt, Int, Float}`. This follows the numeric promotion
lattice: `UInt + UInt → UInt`, `UInt + Int → Int`, `anything + Float → Float`.

**Type mismatch warnings (W009):** After type refinement, the optimizer checks
whether any intrinsic's operand types have zero intersection with the required
types. If so, the result is guaranteed undefined — almost certainly a bug.
Example: `"hello" + 5` warns because `Add` requires numeric but got Text.

**Type cast operator (`as`):** Explicit infallible numeric cast. Both user `as`
casts and compiler-inserted promotions use `Convert(NumericType, ConvertMode)`:
- `Unchecked`: user `as Type` — bit-reinterprets Int↔UInt, always succeeds
- `Checked`: compiler coercion — follows the widening lattice, overflow-checked

| Source | `as UInt` | `as Int` | `as Float` |
|--------|-----------|----------|------------|
| UInt | identity | bit reinterpret | widen |
| Int | bit reinterpret | identity | widen |
| Float | — | — | identity |

- `as Bool`, `as Text`, etc. are **compile-time errors** (E300)
- Float → Int/UInt is not supported — use `floor()`, `round()`, `trunc()`
- Bool/Text/Bytes/Array/Map sources produce undefined at runtime
- Precedence: between unary and multiplicative — `-x as UInt` is `(-x) as UInt`

Lowering: `x as UInt` → `Intrinsic(Convert(UInt, Unchecked), [x])` where the
target type and mode are compile-time properties of the instruction variant,
not runtime operands.

### Control Flow Primitives

Three fundamental control flow terminators, each with a single responsibility:

| Terminator | Purpose | Branches On |
|------------|---------|-------------|
| `If` | Boolean logic | true/false |
| `Match` | Type/structure dispatch | MatchPattern |
| `Jump` | Unconditional | - |

Plus terminators that exit or restart the function:

| Terminator | Purpose |
|------------|---------|
| `Return` | Return value to caller |
| `TailCall` | Self-recursive tail call (overwrite params, jump to entry) |
| `Unreachable` | Placeholder after dead code elimination |

```rust
pub enum Terminator {
    /// Unconditional jump
    Jump { target: BlockId },

    /// Branch on boolean condition
    If {
        condition: VarId,  // Must be Bool
        then_target: BlockId,
        else_target: BlockId,
        span: Span,
    },

    /// Dispatch on type/structure (for type patterns)
    Match {
        value: VarId,
        arms: Vec<(MatchPattern, BlockId)>,
        default: BlockId,
        span: Span,
    },

    /// Return to caller
    Return { value: Option<VarId> },

    /// Unreachable code (placeholder after merging)
    Unreachable,

    /// Self-recursive tail call: overwrite params and jump to entry.
    /// Introduced by the TCO pass; has no successors.
    TailCall { args: Vec<VarId> },
}

/// Pattern for Match terminator arms
pub enum MatchPattern {
    Literal(Literal),      // Match specific value
    Type(BaseType),        // Match simple type (Bool, UInt, Int, Float, Text, Bytes, Map)
    Array(usize),          // Match array with exact length
    ArrayMin(usize),       // Match array with minimum length (rest patterns)
}
```

### Definedness Checking

Definedness checks (`if let`/`if with`) are lowered using `Match` with type-based
dispatch. The compiler uses `Match` to test whether a value is defined, with the
`default` branch handling the undefined case:

```
// Source
if let x = maybe_value {
    use(x);
}

// Lowered IR — Match with all defined types vs default (undefined)
BB0:
    Match(%maybe_value, [(Type(Bool), BB_defined), (Type(UInt), BB_defined), ...], BB_undefined)

BB_defined:
    %x = Copy(%maybe_value)  // x is known non-Undefined here
    // ... use(x) ...
    Jump(BB_continue)

BB_undefined:
    Jump(BB_continue)

BB_continue:
    // execution continues
```

### Pattern Lowering

All pattern matching lowers to combinations of these primitives:

| Construct | Lowers To |
|-----------|-----------|
| `if let x = expr { }` | `Match` (type dispatch) + scoped binding |
| Type patterns (`UInt`, `Text`, etc.) | `Match` with `Type(BaseType)` |
| Array patterns (`[a, b]`) | `Match` with `Array(n)` + `Index` |
| Literal patterns (`42`, `"hello"`) | `Match` with `Literal(value)` |
| Destructuring | `Index` + recursion |
| `if cond { }` | `If` |
| `for x in arr { }` | `Jump` + `If` (bounds) |

Example pattern lowering (statement binding - variables persist):

```
AST: let [a, b] = arr;

IR:
BB0:
    Match(arr, [(Array(2), BB_bind)], BB_fail)

BB_bind:
    %a = Index(arr, 0)
    %b = Index(arr, 1)
    Jump(BB_continue)

BB_fail:
    %a = Undefined
    %b = Undefined
    Jump(BB_continue)

BB_continue:
    // %a, %b are Phi nodes - available for rest of function
```

### Scoped vs Statement Bindings

Bindings come in two forms with different lifetime semantics:

| Binding Type | Lifetime | Fail Path | Use Case |
|--------------|----------|-----------|----------|
| Statement (`let x = expr;`) | Rest of function | Phi node with Undefined | Variables that persist |
| Scoped (`if let`, `if with`, `for`, `match`) | Block only | Never allocated | Temporary bindings |

**Statement bindings** create variables that persist for the rest of the function.
If the pattern fails to match, variables get Undefined values via Phi nodes.

**Scoped bindings** create variables only within a block. The fail path never
allocates these variables — they simply don't exist outside the success block.

Example scoped binding lowering:

```
AST: if let [a, b] = arr { use(a, b); }

IR:
BB0:
    Match(arr, [(Array(2), BB_bind)], BB_else)

BB_bind:
    %a = Index(arr, 0)
    %b = Index(arr, 1)
    // ... body uses %a, %b ...
    Jump(BB_continue)

BB_else:
    // %a, %b never allocated here
    Jump(BB_continue)

BB_continue:
    // %a, %b not accessible — scoped to BB_bind
```

Scoped bindings apply to:

- `if let pattern = expr { }`
- `if with pattern = expr { }`
- `for x in arr { }` / `for let x in arr { }`
- `match expr { pattern => { } }`

---

## Expression Semantics

### Expressions vs Statements

Rill minimizes the distinction between expressions and statements. The key insight:
**a statement is just an expression whose result is discarded**.

| Construct | Type | Evaluates To |
|-----------|------|--------------|
| `x + 1` | Expression | Computed value |
| `x = 5` | Expression | Assigned value (or undefined if lvalue invalid) |
| `expr;` | Statement | Discards result |
| `{ stmts; expr }` | Block | Last expression without `;` is the return value |
| `{ stmts; }` | Block | No final expression → undefined |
| `{ }` | Block | Empty → undefined |

### Assignment as Expression

Assignment is an expression that returns the assigned value:

```rust
let y = (x = 5);        // y is 5, x is 5
let z = (arr[i] = v);   // z is v if succeeded, undefined if lvalue invalid

// Chained assignment (right-associative)
a = b = c = 0;          // All set to 0, evaluates to 0
```

This enables **checked assignment** for potentially-undefined lvalues:

```rust
// Unchecked - value may vanish silently if arr[i] is undefined
arr[i] = v;

// Checked - capture result to detect failure
if let result = (arr[i] = v) {
    // Assignment succeeded
} else {
    // lvalue was undefined (out of bounds, etc.)
}

// Alternative - verify lvalue exists first
if with slot = arr[i] {
    slot = v;  // Guaranteed to succeed
}
```

### Lvalue Validity

Not all expressions are valid lvalues:

| Expression | Valid Lvalue? | Notes |
|------------|---------------|-------|
| `x` | Yes | Simple variable |
| `arr[i]` | Yes | Array index (may be undefined if OOB) |
| `obj.field` | Yes | Member access (may be undefined) |
| `x @ b` | Yes | Bit access (may be undefined if b >= 64) |
| `x + 1` | No | Arithmetic result has no location |
| `foo()` | No | Function result has no location |

When an lvalue evaluates to undefined (e.g., out-of-bounds index), the assignment
becomes a no-op and the expression evaluates to undefined.

### Short-Circuit Evaluation

Assignment to potentially-undefined lvalues uses **short-circuit evaluation**:
the rhs is only evaluated if the lvalue is defined.

```rust
arr[100] = expensive();  // expensive() NOT called if arr[100] is OOB
x @ 128 = compute();     // compute() NOT called if bit 128 is invalid
```

This is consistent with `&&` and `||` short-circuit behavior and avoids wasted
computation when assigning to invalid locations. The generated IR uses Match
terminators to check lvalue validity before evaluating the rhs.

### Type Cast (`as`)

The `as` operator performs infallible numeric reinterpretation or widening:

```rill
let unsigned = -1 as UInt;       // bit reinterpret: 2^64-1
let signed = max_uint as Int;    // bit reinterpret: -1
let precise = counter as Float;  // widen to float
```

Key properties:
- **Infallible** for valid numeric pairs — always produces a value
- **No implicit truncation** — Float→Int requires explicit `floor()`/`round()`/`trunc()`
- **Compile-time validated** — invalid targets like `as Bool` or `as Text` are E300 errors
- **Distinct from type patterns** — `UInt(x)` tests if a value *is* UInt; `x as UInt` *makes* it UInt

Precedence is between unary operators and multiplicative, so:
- `-x as UInt` parses as `(-x) as UInt`
- `x + y as Float` parses as `x + (y as Float)`
- `x as Int as UInt` chains left-to-right: `(x as Int) as UInt`

### Semicolons and Blocks

The semicolon `;` marks an expression as a statement (value discarded).
The last expression in a block without `;` becomes the block's return value.
Control-flow expressions (`if`, `while`, `loop`, `for`, `match`) can appear
mid-block without `;` — they are void statements. At the end of a block,
they become the return value.

```rust
fn example() { 42 }                    // returns 42

fn example() {
    if cond { 1 } else { 2 }           // if-expression as return value
}

fn example() {
    do_stuff();                         // expression statement (;)
    if cond { handle() }                // void statement (mid-block, no ;)
    result                              // final expression (return value)
}

fn example() {
    let x = 5;                          // binding declaration
}                                       // no final expression → undefined
```

**No Unit type needed** — undefined serves as "absence of meaningful value" uniformly.

### Binding Declarations vs Expressions

`let` and `with` are **binding declarations**, not expressions:

- They introduce names into scope (a side effect on the environment)
- They don't evaluate to a value themselves
- `if let`/`if with` is special syntax, not `let` being used as an expression

```rust
let x = 5;              // Declaration - introduces x
let y = (let z = 5);    // ERROR: let is not an expression

if let x = maybe {      // Special syntax - conditional binding
    use(x);
}
```

This keeps the language simple: bindings affect scope, assignments compute values.

---

## Bit Test/Set Operator

The `@` operator provides efficient bit-level access to unsigned integers:

### Syntax

```rust
value @ bit           // Test: is bit set?
value @ bit = bool    // Set: set or clear bit
```

### Semantics

| Operation | Result |
|-----------|--------|
| `x @ b` (read) | `true` if bit b is set, `false` if clear |
| `x @ b = true` | Sets bit b |
| `x @ b = false` | Clears bit b |
| `x @ b` where b >= 64 | `undefined` (out of range) |
| `x @ b` where x or b not UInt | `undefined` (type error) |

### Design Rationale

The `@` operator is conceptually **syntactic sugar for bit-array access**:

- Semantics match array indexing: out-of-range returns undefined
- Valid as both rvalue (test) and lvalue (set/clear)
- No auto-extension: you can't set bit 128 of a 64-bit integer

### Examples

```rust
let flags = 0b1010;

flags @ 1              // true (bit 1 is set)
flags @ 0              // false (bit 0 is clear)

flags @ 2 = true;      // Set bit 2: flags = 0b1110
flags @ 3 = false;     // Clear bit 3: flags = 0b0110

// Compound assignment for toggle
flags @ 1 ^= true;     // Toggle bit 1

// Checked bit access
if let result = (flags @ b = true) {
    // Bit set succeeded
}

// Out of range
flags @ 100            // undefined
flags @ 100 = true;    // No-op, assignment evaluates to undefined
```

### Implementation

The `@` operator lowers to intrinsics:

- Read: `Intrinsic(BitTest, [value, bit])` → Bool or undefined
- Write: `Intrinsic(BitSet, [value, bit, bool])` → UInt or undefined

---

## Optimization Pipeline

The IR goes through a series of optimization passes after lowering. The passes are
organized into two phases: **coarse** (before type info) and **type-informed**
(on the simplified CFG after guard elimination).

### Pass Overview

```
IR (lowered, SSA-promoted)
    │
    │  ── Unified Optimization Fixpoint ──
    ▼
┌──────────────────────────────────────┐
│  Constant Folding                    │
│  Common Subexpression Elimination    │
│  Copy Propagation                    │
│  Dead Code Elimination               │
│  Ref Elision                         │
│  Coercion Elision                    │
│  CFG Simplification                  │  Jump threading, Phi simplification
│  ─── type analysis ───               │
│  Type Refinement                     │  Intrinsic-aware: Add(UInt,UInt) → {UInt}
│  Coercion Insertion                  │  Insert Convert(Checked) for mixed types
│  Convert Elision                     │  Identity Convert → Copy
│  Algebraic Simplification            │  x+0→x, x*1→x, x*0→0, !!x→x
│  Condition Folding                   │  Non-Bool If condition → Jump(else)
│  Dead Arm Elimination                │  Prune Match arms, collapse to Jump
│             │ ◄── repeat while       │
│             │     any pass changed   │
└─────────────┴────────────────────────┘
    │
    │  ── Diagnostics ──
    ▼
  W009 type mismatch, W201 definedness (on final converged IR)
    │
    │  ── Phase B: Interprocedural ──
    ▼
┌─────────────────────┐
│ M: Monomorphization │  Clone functions with >1 distinct call-site type
│                     │  signature. Max 4 variants, skip recursive fns.
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ B1: Interprocedural │  Collect arg types + definedness from all call
│     Analysis        │  sites. Infer function purity (optimistic fixpoint).
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ B2: Return Type Inf │  Iterate until stable with narrowed params.
│                     │  Infer return TypeSets from Return terminators.
│                     │  Handles forward refs, recursion, mutual recursion.
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ B3: Re-optimize     │  Re-run unified fixpoint on functions with
│                     │  narrowed params/returns. Purity-aware DCE.
└──────────┬──────────┘
           ▼
IR (optimized)
```

### Fixpoint Iteration

All optimization passes — both data-flow (const fold, CSE, copy prop, DCE,
ref elision, coercion elision, CFG simplify) and type-informed (type refinement,
coercion insertion, cast elision, algebraic simplification, condition folding,
dead arm elimination) — run in a single unified loop until no pass makes changes.
This avoids phase-ordering issues where type-informed passes expose new data-flow
opportunities and vice versa. Cascading effects:

- Const fold may turn a Phi into a constant → type refinement sees single type
- Dead arm elimination removes guard Matches → single-source Phi → copy prop
- CSE deduplicates identical Phis and intrinsic operations
- Ref elision demotes read-only MakeRefs → exposes Copy/Index for const fold
- DCE removes dead instructions → CFG simplify removes dead blocks
- Jump threading redirects through trivial blocks → Phi simplification
- CFG simplify may remove WriteRefs → ref elision demotes more MakeRefs

Typically converges in 2-3 iterations. Diagnostics (W009/W200/W201) are emitted
once after final convergence so that dead code elimination has a chance to
remove synthetic instructions before warnings are generated.

### Type-Informed Definedness

The coercion insertion pass bridges type analysis into definedness. Within the
unified fixpoint, this happens naturally across iterations:

1. **Early iterations**: Without type info, `Add(Text, UInt)` is conservatively
   `MaybeDefined` — Add *can* fail, but we don't know from types alone that
   it *always* fails.

2. **After type refinement + coercion insertion**: The coercion pass consults
   TypeAnalysis and emits explicit `Instruction::Undefined` for invalid type
   combinations. On the next fixpoint iteration, constant folding and DCE see
   these Undefined instructions → proves `Undefined` instead of `MaybeDefined`
   → dead arm elimination removes dead branches. No separate pass needed —
   the unified loop handles the cascading naturally.

### Pass 1: Early Constant Folding

**Goal:** Fold obvious compile-time constants before analysis.

This pass runs first to simplify the IR before analysis passes. It evaluates
intrinsic operations (via `eval_intrinsic_const`) when all arguments are literal
constants, replacing them with `Const` instructions.

**Transformations:**

- `Intrinsic(Add, [Const(1), Const(2)])` → `Const(3)`
- `Intrinsic(Eq, [Const(true), Const(false)])` → `Const(false)`
- Constant If conditions → `Jump` to appropriate target

Running constant folding early simplifies the CFG for subsequent analysis passes.

### Pass 1.5: Ref Elision

**Goal:** Eliminate unnecessary `MakeRef` indirection.

`MakeRef` instructions create explicit reference bindings for `with` semantics.
Many of these are read-only — the variable is never written through (`WriteRef`).
In those cases the runtime `Slot::Ref` indirection is pure overhead. This pass
demotes them to cheaper instructions.

**Three rewrites:**

| Rewrite | Condition | Before → After |
|---------|-----------|----------------|
| Ref chain shortening | `base` from `MakeRef(_, orig)` | `MakeRef(d, base)` → `MakeRef(d, orig)` |
| Accessor demotion | No `WriteRef`/`WriteAccessor` targets `dest` | `MakeAccessor(d, b, k)` → `Index(d, b, k)` |
| Ref demotion | No `WriteRef` targets `dest` AND base not in `written_bases` | `MakeRef(d, b)` → `Copy(d, b)` |

**Written bases:** A base is "written" if any `WriteAccessor` or `WriteRef`
in the function modifies it. The pass follows `MakeRef` chains to find the
resolved base. If a sibling ref to the same base has a write-back, the
`Slot::Ref` alias must stay live so reads see the mutation.

**Example:** `with x = arr; with y = arr[0]; y = 10` — the `WriteAccessor`
for `arr[0]` writes to `arr`, so `arr` is in `written_bases`. If another ref
aliases `arr` via `MakeRef(_, arr)`, it cannot be demoted to `Copy` because
it must see the mutation.

**Interaction with fixpoint:** Runs after constant folding. As other passes
remove dead code (unreachable blocks, eliminated guards), `WriteRef`
instructions may become unreachable. On the next fixpoint iteration, ref
elision sees fewer `WriteRef`s and can demote more `MakeRef`s.

### Pass 2: Definedness Analysis (Coarse)

**Goal:** Determine which variables are provably defined (not Undefined).

This is the coarse pass — it uses `IntrinsicOp::is_fallible()` to determine whether
an operation might return undefined, but has no type information. `Add(Text, UInt)`
is conservatively `MaybeDefined` (Add *can* fail), not `Undefined` (it *always* fails
for these types). The fine-grained pass after coercion insertion (planned) will
tighten this.

Definedness is orthogonal to type analysis - a value can be "definitely defined"
without knowing its concrete type, and vice versa. Definedness flows from sources
(literals, constants) through operations and merges at control flow joins.

**Lattice:**

```
         Defined
           │
           │
      MaybeDefined
           │
           │
        Undefined
```

- `Defined`: Value is guaranteed non-Undefined
- `MaybeDefined`: Value might be Undefined (need runtime check)
- `Undefined`: Value is guaranteed Undefined

**Transfer rules:**

| Instruction | Result Definedness |
|-------------|-------------------|
| `Const { dest, .. }` | `Defined` |
| `Undefined { dest }` | `Undefined` |
| `Copy { dest, src }` | inherits from `src` |
| `Index { dest, base, key }` | `Defined` if guarded, else `MaybeDefined` |
| `MakeAccessor { dest, base, key }` | `Defined` if guarded, else `MaybeDefined` |
| `MakeRef { dest, base }` | inherits from `base` |
| `WriteRef { .. }` | no dest (side effect only) |
| `Intrinsic { op, .. }` infallible, all args Defined | `Defined` |
| `Intrinsic { op, .. }` fallible or args MaybeDefined | `MaybeDefined` |
| `Call { dest, function, .. }` | depends on `ExternMeta.purity` |
| `Phi { dest, sources }` | meet of all sources |

**Control flow refinement:**

At a `Match` terminator:
- In each arm block: scrutinee is `Defined` (it matched a pattern)
- In the default block: no refinement (value may be undefined)

**Guarded index analysis** (`collect_guarded_indices` pre-pass):

Index and element MakeRef operations are marked `Defined` (not OOB) when
protected by a bounds check in the predecessor block:

| Guard pattern | What's safe |
|---|---|
| `If(Lt(i, Len(base)), body)` | `Index(base, i)` in body — for-loop pattern |
| `If(Not(Lt(Len(base), N)), body)` | `Index(base, const<N)` in body — `len >= N` |
| `If(Lt(N-1, Len(base)), body)` | `Index(base, const<N)` in body — `len > N-1` |

This eliminates spurious E201 warnings for guarded array access, including
packet processing patterns like `if len(packet) >= 4 { packet[0] ... packet[3] }`.

**Why flow-sensitive analysis in SSA form?**

In SSA, each variable is assigned exactly once, so one might expect each variable
to have a single fixed definedness. However, Match arms create contexts where we
know more than the variable's "intrinsic" definedness:

```
Block 0:
  v0 = param           // Intrinsic: MaybeDefined (caller might pass undefined)
  Match v0 -> [(Type(UInt), B1), ...], default: B2

Block 1:               // After Match, we KNOW v0 is Defined
  v1 = v0 + 1          // v1 is Defined (both operands are Defined)
  // Subsequent definedness checks on v1 can be eliminated
```

Without flow-sensitivity, `v0` stays `MaybeDefined` everywhere, so `v1 = v0 + 1`
would compute as `MaybeDefined`, and we couldn't optimize subsequent checks.

With flow-sensitivity, after the Match's arm branch, we track that `v0` is
`Defined` in that context, so `v1` becomes `Defined`, enabling dead arm elimination.

The analysis tracks definedness at block entry/exit points, propagating refined
knowledge through the CFG. This is a forward dataflow analysis with the meet
operation computing the most conservative (lowest) definedness at join points.

**Output:** Map of `(BlockId, VarId) → Definedness` (definedness at block entry/exit)

### Pass 2.5: Definedness Diagnostics

**Goal:** Emit warnings and errors based on definedness analysis.

Walks the IR and checks each instruction's operands against the computed
definedness state. Runs before guard elimination reshapes the control flow,
so provenance chains (tracing back to the root cause of undefined-ness) are
still intact.

**Checks:**

| Context | Definitely Undefined | Maybe Undefined |
|---------|---------------------|-----------------|
| Control flow (`if` condition, `match` scrutinee) | **W200** warning | **W201** warning |
| Data flow (intrinsic arg, index base/key, etc.) | **W200** warning | **W201** warning |

**Provenance tracking:** Each diagnostic includes the root cause — where the
undefined value originated. Traces propagation chains through Copy/Phi back to
the source (a fallible Call, an Index operation, etc.).

Example:
```
warning[E201]: use of possibly undefined value `_5` as argument 1 to
    intrinsic `Add` in function `process`
  --> src:12:5
  = note: value originates from call to `parse_input`
```

### CFG Simplification

**Goal:** Simplify the control flow graph after other passes remove code.

- Merge single-predecessor/single-successor blocks
- Remove unreachable blocks (no predecessors)
- Eliminate trivial jumps (jump to next block)
- Fold If terminators with constant conditions → Jump

### Pass 4: Type Refinement

**Goal:** Narrow the `types` set in each variable's TypeSet.

This runs after Phase 1 has simplified the CFG. Type refinement tracks
the possible concrete types (Bool, UInt, Int, etc.) at each program point.

**Lattice:** Powerset of `{Bool, UInt, Int, Float, Text, Bytes, Array, Map, Sequence, Undefined}`

Meet = intersection (narrowing), Join = union (at Phi nodes)

**Transfer rules:**

| Instruction | Result Types |
|-------------|--------------|
| `Const { value: Literal::Bool(_), .. }` | `{Bool}` |
| `Const { value: Literal::UInt(_), .. }` | `{UInt}` |
| `Intrinsic { op: Add, args: [UInt, UInt] }` | `{UInt}` (via promotion lattice) |
| `Intrinsic { op: Add, args: [UInt, Int] }` | `{Int}` (promoted) |
| `Intrinsic { op: Add, args: [?, Float] }` | `{Float}` (promoted) |
| `Intrinsic { op: Eq, .. }` | `{Bool}` |
| `Intrinsic { op: Len, .. }` | `{UInt}` |
| `Intrinsic { op: MakeArray, .. }` | `{Array}` |
| `Index { .. }` | depends on base type |
| `MakeRef { .. }` | all types (ref target could be any type) |
| `WriteRef { .. }` | no dest (side effect only) |
| `Phi { .. }` | union of source types |

**Intrinsic-aware refinement:** The pass calls `op.result_type_refined(arg_types)`
which uses the numeric promotion lattice to produce precise result types.
`Add(UInt, UInt)` → `{UInt}`, not `{UInt, Int, Float}`.

**Numeric promotion lattice:** `UInt ⊂ Int ⊂ Float`
- Same type → same type: `UInt + UInt → UInt`
- Mixed integers → Int: `UInt + Int → Int`
- Anything + Float → Float: `Int + Float → Float`

**Control flow refinement:**

At a `Match` terminator with `Type(t)` pattern:
- In the matching arm: value has type `{t}`

### Pass 4.5: Type Mismatch Diagnostics

After type refinement, the optimizer checks each `Intrinsic` instruction:
if any argument's refined type has zero intersection with the required type
(`IntrinsicOp::param_type()`), the result is guaranteed undefined. This emits
a W009 warning — almost certainly a user bug.

Example: `"hello" + 5` — `Add` requires numeric args, but Text has no
intersection with `{UInt, Int, Float}` → W009.

**Optimizations enabled by type refinement:**
- Remove impossible Match arms (type not in TypeSet)
- Specialize polymorphic operations when type is known
- Future: coercion insertion generates guard trees using refined types

### Pass 5: Cleanup Constant Folding

**Goal:** Fold constants exposed by earlier passes.

After CFG simplification, new constant folding opportunities
may emerge. This pass runs the same constant folding logic as Pass 1 to clean up.

**Transformations:**

- Fold `Intrinsic` ops with const args: `Intrinsic(Add, [1, 2])` → `Const(3)`
- Fold `Call` to const externs with const args
- Simplify `If` terminators: `If { condition: Const(true), .. }` → `Jump { target: then }`
- Replace variable references with `Const` instructions when value is known

### Dead Code Elimination

**Goal:** Remove computations whose results are never used.

**Algorithm:**

1. Mark as "live":
   - Variables used in `Return`, `TailCall` terminators
   - Variables used in `WriteAccessor` or `WriteRef` (side effects)
   - Variables used in impure `Call` arguments
   - Variables used in terminator conditions (`If`, `Match`)

2. Propagate liveness backwards:
   - If `dest` is live, mark all variables used in that instruction as live

3. Remove dead instructions:
   - Instructions whose `dest` is not live

4. Remove unreachable blocks:
   - Blocks with no predecessors (after simplification)

**Order matters:** Run DCE after constant propagation. When constants fold away
branches, more code becomes unreachable.

### File Structure

```
src/opt/
├── mod.rs             # Pipeline orchestration, optimize(), definedness/diagnostics
├── const_fold.rs      # Constant folding (intrinsic const-eval)
├── cse.rs             # Common subexpression elimination
├── copy_prop.rs       # Copy propagation
├── dce.rs             # Dead code elimination
├── ref_elision.rs     # Ref elision (MakeRef → Copy, MakeAccessor → Index)
├── coercion.rs        # Coercion insertion (Convert + Undefined) and elision
├── cfg_simplify.rs    # CFG simplification (merge blocks, remove unreachable)
├── type_refinement.rs # Type refinement + interprocedural analysis
├── cast_elision.rs    # Identity Convert → Copy
├── algebra.rs         # Algebraic simplification (x+0→x, x*1→x, etc.)
├── tail_call.rs       # Tail-call optimization (Call+Return → TailCall)
```

### Fixed-Point Iteration

Some passes may enable further optimizations by others. The pipeline can iterate:

```rust
loop {
    let mut changed = 0;
    changed += fold_constants(function, externs, diagnostics);
    changed += eliminate_common_subexpressions(function);
    changed += propagate_copies(function);
    changed += eliminate_dead_code(function);
    changed += elide_refs(function);
    changed += elide_coercions(function);
    changed += simplify_cfg(function);  // includes jump threading + Phi simplification

    let types = analyze_types(function, Some(externs));
    changed += insert_coercions(function, &types);
    changed += elide_identity_casts(function, &types);
    changed += simplify_algebra(function, &types);
    changed += fold_non_bool_conditions(function, &types);
    changed += eliminate_dead_match_arms(function, &types);

    if changed == 0 { break; }
}
```

In practice, 2-3 iterations suffice for most programs.

---

## Compiler Diagnostics

The compiler uses the definedness analysis to emit warnings and errors at compile
time. This catches bugs early without runtime overhead.

### Definedness-Based Diagnostics

The definedness lattice (`Defined`, `MaybeDefined`, `Undefined`) enables precise
diagnostics about value presence.

#### Warnings

Warnings indicate code that may have issues but is still valid:

```rust
// Warning: unchecked assignment to potentially-undefined location
arr[i] = v;

// Warning: value may be undefined; consider using `if let`
let y = x + 1;  // if x might be undefined
```

**Suppressing warnings:**

```rust
// Explicit check
if let result = (arr[i] = v) { }

// Explicit discard
let _ = arr[i] = v;

// Guard first
if with slot = arr[i] {
    slot = v;
}
```

#### Errors

Errors indicate code that is definitely wrong:

```rust
// Error: value is definitely undefined
let x = undefined_fn();
let y = x + 1;  // x is Undefined, not MaybeDefined

// Error: assignment to definitely-undefined location has no effect
let missing;
missing = 5;  // 'missing' is always Undefined here
```

### Unchecked Assignment Warning

The compiler emits warnings for assignments to potentially-undefined lvalues
when the result is not checked:

```rust
arr[i] = v;      // ⚠️ Warning: Unchecked assignment, destination may be undefined
x @ b = true;    // ⚠️ Warning: Unchecked assignment, destination may be undefined
```

**Safe alternatives (no warning):**

```rust
// Check result
if let _ = (arr[i] = v) { }
if is_defined(arr[i] = v) { }

// Explicit discard
let _ = arr[i] = v;

// Verify lvalue first
if with slot = arr[i] {
    slot = v;
}
```

**Rationale:** This catches "black holes" where data silently vanishes. The warning
doesn't prevent the code from compiling - it's the programmer's choice to ignore
or address it.

---

## Language Features

### Binding Modes

The language uses a consistent pattern: **default is by-value** (CoW makes clones cheap),
use `with` to opt into by-reference when mutation propagation is needed.
The `let` keyword is redundant but can be used explicitly for clarity.

| Context | by-value (default) | by-value (explicit) | by-ref |
|---------|-------------------|---------------------|--------|
| Statement | — | `let x = expr` | `with x = expr` |
| Conditional | — | `if let x = expr { }` | `if with x = expr { }` |
| For loop (single) | `for x in arr { }` | `for let x in arr { }` | `for with x in arr { }` |
| For loop (pair) | `for k, v in map { }` | `for let k, v in map { }` | `for with k, v in map { }` |
| Match arm | `pat => { }` | `let pat => { }` | `with pat => { }` |
| Function param | `fn foo(x)` | `fn foo(let x)` | `fn foo(with x)` |

**For-loop pair binding:**

```rust
for k, v in map { }         // k = key (always by-val), v refs value
for i, x in arr { }         // i = index (always by-val), x refs element
for let k, v in map { }     // both by-value
```

The first variable (key/index) is always by-value. The `let`/`with` keyword
controls the second variable's binding mode.

**For-loop and Sequences:**

Collections (Array, Map, Bytes) support by-ref iteration — mutations through the
loop variable write back to the source. Sequences (from `..` operator) are always
by-value — there is no backing store to write back to. Text iteration yields
characters by-value (characters aren't individually mutable slots).

The compiler warns on mutations to non-ref-backed loop variables (dead stores).

**Semantics:**

- **By-value** (default or `let`): Variable is a copy (CoW clone — Rc increment); mutations are local only
- **By-reference** (`with`): Variable refers to original location; mutations flow back to source

**Why allow explicit `with`?** Self-documenting code. When you write `fn process(with data)`,
it signals intent: "I will mutate this parameter." The explicit keyword is optional but encouraged
for clarity.

**IR-level reference architecture:**

Reference semantics are explicit in the IR via two instructions, making them
visible to the optimizer (no hidden aliasing):

| Instruction | Emitted When | Runtime Effect |
|-------------|-------------|----------------|
| `MakeAccessor { dest, base, key }` | `with x = arr[i]`, for-loop by-ref | Creates `Slot::Accessor { base, key }` (far pointer) |
| `MakeRef { dest, base }` | `with x = y`, by-ref function params | Creates `Slot::Ref(target)` (near pointer) |
| `WriteAccessor { base, key, value }` | Direct `arr[i] = val` | Type-specialized `set_array_elem` or `set_map_entry` |
| `WriteRef { ref_var, value }` | Assignment to `with`-bound variable | `vm.set_local` resolves through Slot::Ref / Slot::Accessor |

The lowerer tracks ref origins in a scoped `HashMap<Identifier, RefOrigin>`
(managed alongside the scope stack). When a ref-backed variable is assigned
(`x = 10`), the lowerer:
1. Looks up `x` in `ref_origins` → finds `RefOrigin { ref_var, base_var, key_var }`
2. If `key_var` is Some: emits `WriteAccessor { base, key, value }` (element write)
3. If `key_var` is None: emits `WriteRef { ref_var, value }` (whole-value write)
4. Emits `Reload(base_var)` + reassigns the base variable (SSA visibility)

The optimizer can then:
- See `MakeRef` and know which variables are references and to what
- See `WriteRef` and know which collections are mutated through references
- Eliminate dead `WriteRef` (collection never read after write-back)
- Forward values through `WriteRef` (a read after write-back returns the written value)
- Reduce `MakeRef` → `Index` when no `WriteRef` uses it (read-only ref)

Pattern destructuring with `with` (`with [a, b] = arr`) emits `MakeRef` for
each element instead of `Index`, propagating ref origins to each bound variable.

### Variadic Functions (Rest Parameters)

Functions can accept variable arguments using the rest parameter syntax `..name`:

```rust
fn printf(format, ..args) {
    // args is an Array containing all excess arguments
    for arg in args {
        // process each argument
    }
}

printf("hello");                    // args = []
printf("hello %s", name);           // args = [name]
printf("hello %s %d", name, age);   // args = [name, age]
```

Rest parameters follow the same binding mode rules:

- `..args` - by-value (default)
- `let ..args` - by-value (explicit)
- `with ..args` - by-reference

The rest parameter must be the last parameter in the function signature. At the call site,
excess arguments are collected into an Array and passed as the rest parameter.

### Type Patterns

```rust
// Type check without binding
match x { UInt => { }, _ => { } }

// Type check with binding
match x { UInt(n) => { use(n) }, _ => { } }

// Type narrowing reference
if with UInt(n) = record.priority {
    n += 1;  // Mutates record.priority
}
```

### Prelude Functions (convention)

Utility functions typically provided as a `prelude.rill` source file.
Scripts import them with `import "prelude.rill" as _;`.

**Type checking** (regular Rill code, inlined by the optimizer):

| Function | Returns | Compiles To |
|----------|---------|-------------|
| `is_defined(v)` | `Bool` | `Match` (type dispatch) + Phi |
| `is_uint(v)`, `is_int(v)`, ... | `Bool` | `Match` + Phi |
| `to_uint(v)`, `to_int(v)`, ... | Value or Undefined | Type conversion |
| `default(v, fallback)` | Value | `Match` + Phi |

**Core intrinsics** callable by name (not prelude — hard-coded in compiler):

| Function | Returns | Purpose |
|----------|---------|---------|
| `len(v)` | `UInt` or Undefined | Collection/sequence length |
| `collect(seq)` | Array | Materialize a sequence into an Array |

**Core intrinsics** not callable by name (lowering targets for syntax):

| IntrinsicOp | Used For |
|-------------|----------|
| `MakeArray` | `[a, b, c]` literals |
| `MakeMap` | `{k: v}` literals |
| `MakeSeq` | `start..end` ranges |
| `ArraySeq` | `..rest` patterns |

### Rest Patterns

```rust
let [first, ..rest] = arr;      // rest = Sequence (immutable, by-value iteration)
with [first, ..rest] = arr;     // first = ref, rest = Sequence (mutable, write-back)
let [head, .., tail] = arr;     // ignore middle
let [a, ..middle, z] = arr;     // capture middle as Sequence
let [first, ..] = arr;          // ignore rest (no binding)
```

The `..rest` variable is always a Sequence (zero-copy view of the source array),
never a copied Array. Mutability follows the binding mode:

- `let [a, ..rest] = arr` → rest is an immutable Sequence; iteration is by-value
- `with [a, ..rest] = arr` → rest is a mutable Sequence; for-loop write-back works

Use `collect(rest)` to materialize a Sequence into a concrete Array if random
access is needed.

### Sequences (the `..` operator)

The `..` and `..=` operators create lazy sequences with O(1) memory:

```rust
0..10     // Exclusive: yields 0, 1, ..., 9
0..=10    // Inclusive: yields 0, 1, ..., 10

for i in 0..len(arr) { }        // Dynamic bounds
let s = 0..5;                   // Store a sequence
for x in s { }                  // Consume it
let arr = collect(0..10);       // Materialize to Array
```

Sequences are an internal type — not user-visible for pattern matching. Users
never write "Sequence" in their code. They write `0..10`, use `for` loops,
and call `collect()`.

Host externs can return sequences for lazy data streams (e.g., iterating over
records in a database cursor or pages in a document without materializing
them all into an Array).

---

## Error Handling

### Execution Errors

| Error | Cause | Recovery |
|-------|-------|----------|
| `StackOverflow` | Deep recursion or large frames | None (abort) |
| `HeapOverflow` | Too many allocations | None (abort) |

### Undefined Propagation

Everything else returns Undefined:

- Type mismatch in extern
- Division by zero
- Arithmetic overflow/underflow
- Out of bounds index
- Failed map lookup
- Invalid type conversion

Scripts handle with:

```rust
if is_defined(x) { use(x) }  // Existence check
if let v = to_uint(x) { }    // Conditional binding
let y = x;                   // Undefined propagates through operations
```

---

## Module System

Rill's module system is designed for **convenience of consumption**, not for
building large hierarchical module trees. The target user is a devops or
network admin writing a script that reuses functions from other scripts on
disk. There are no chained namespaces, no re-exports, and no namespace
declaration syntax.

Two separate mechanisms exist for two separate purposes:

| Keyword | Purpose | What it does |
|---------|---------|--------------|
| `import` | Source file reuse | Loads and compiles a `.rill` file, creates a namespace |
| `require` | Extern dependency | Declares that the script needs an embedder-provided namespace |

### Source File Imports

`import` loads a Rill source file and makes its functions and constants
available under a namespace. The namespace name is derived from the filename
stem (without extension or path), or can be overridden with `as`:

```rill
import "./helpers.rill";                // namespace `helpers`
import "../common/validation.rill";     // namespace `validation`
import "./helpers.rill" as h;           // namespace `h` (explicit alias)
import "./utils.rill" as _;            // no namespace — functions available unqualified
```

At call sites, use `namespace::name` (or just `name` for `as _` imports):

```rill
helpers::compute_checksum(data)
validation::check(record)
h::compute_checksum(data)
my_util_function(x)                    // from `as _` import — no prefix
```

If two imports derive the same namespace name (e.g., two files both named
`utils.rill`), the compiler emits an error requiring one to use an `as` alias.

### Extern Dependencies

`require` declares that the script depends on an extern namespace provided by
the embedder. It does not introduce the namespace — the embedder does that by
registering functions into the namespace. The `require` statement documents
the dependency and enables the compiler to validate it:

```rill
require cbor;                   // needs extern namespace `cbor`
require cbor as c;              // same, aliased to `c`
require bpsec;                  // needs extern namespace `bpsec`
require encoding as _;          // functions available unqualified
```

At call sites:

```rill
cbor::decode(bytes)
c::decode(bytes)                // if aliased
bpsec::validate(block, bundle)
hex_encode(data)                // from `as _` require — no prefix
```

If the embedder has not registered the required namespace, the compiler emits
a clear error: "extern namespace `cbor` not provided by embedder". This is
much better than a cryptic "undefined function `cbor::decode`" at link time.

Externs without a namespace (registered globally) are always available without
a `require` statement — they are called unqualified, like intrinsics.

### Visibility

Function and constant visibility is **structural**, not declarative — there is
no `pub` keyword.

| Declared in | Visibility | Callable by embedder? | DCE eligible? |
|-------------|------------|----------------------|---------------|
| Root file | Public | Yes (`FunctionHandle`) | No — always kept |
| Imported file | Private | No | Yes — removed if unused |

The root file is the file passed to `compile()`. Everything declared directly
in it is a potential entry point for the embedder. Imported files provide
helper functions and constants that are implementation details.

```rill
// root.rill — all functions here are public
require cbor;
import "./helpers.rill";

fn process(data) {
    let decoded = cbor::decode(data);
    helpers::validate(decoded)
}

// helpers.rill — all functions here are private
fn validate(record) { ... }    // private — only callable via helpers::validate
fn internal() { ... }          // private — if unused, eliminated by DCE
```

**Imports are private to the importing file.** If `root.rill` imports
`helpers.rill`, and `helpers.rill` imports `utils.rill`, root cannot see
`utils::*`. Each file is a self-contained compilation unit with its own
import and require declarations. There is no re-export mechanism.

### Name Resolution

**Qualified calls** (`ns::func()`):

1. Extern namespaces (from `require` declarations)
2. Imported source modules (from `import` declarations)

If a namespace alias from `import` collides with one from `require` (or
with another `import`), the compiler emits an error.

**Unqualified calls** (`func()`):

1. Intrinsics — `len()`, `collect()`, `append()` (compiler built-in, cannot be shadowed)
2. Local user functions — defined in the same source file
3. Merged imports — from `import "file" as _`
4. Merged externs — from `require ns as _`

Local functions and imports shadow externs — a warning is emitted.
The original is always reachable via qualified `ns::func()` syntax.

### Name Clash Rules

| Clash | Behavior |
|-------|----------|
| Function/constant name vs intrinsic (`len`, `collect`) | Error |
| Duplicate function or constant name in the same file | Error |
| Duplicate namespace alias (import vs import, import vs require) | Error |
| Local function shadows merged import or extern | Warning |
| Merged import shadows merged extern | Warning |

### Source Loader

The compiler does not hardcode file I/O. Instead, the embedder provides a
**source loader** — a trait implementation that handles all source
resolution. This keeps the compiler platform-agnostic (works in no-std
environments, WASM, etc.).

The `SourceLoader` trait resolves import paths to source text:

```rust
pub trait SourceLoader {
    /// Load source text.
    /// `identifier` is the import path (e.g., "utils.rill").
    /// `from` is the canonical_id of the importing file (None for root).
    fn load(&self, identifier: &str, from: Option<&str>) -> Result<SourceResult, String>;
}

pub struct SourceResult {
    pub source: String,        // UTF-8 source text
    pub namespace: String,     // default namespace for this module
    pub canonical_id: String,  // unique identity for deduplication
}
```

The `Compiler` builder takes a single loader at construction:

```rust
let loader = FileLoader::new("./scripts");
let mut compiler = Compiler::new(&loader);
compiler.add_extern(ExternDef::new("math", "sqrt", sqrt_impl))?;
compiler.add("main.rill");
let (program, warnings) = compiler.build()?;
```

Provided implementations: `FileLoader` (filesystem, resolves relative
paths, canonical = absolute path) and `MemoryLoader` (in-memory map
for testing/embedding).

The only core intrinsics that are user-callable by name are `len()`,
`collect()`, and `append()`, which the compiler recognizes in
`try_lower_intrinsic`. These cannot be shadowed by user definitions
(detected at definition time).

---

## Core Intrinsics and Externs

| Concept | Definition | When Evaluated | Registered? |
|---------|------------|----------------|-------------|
| **Core intrinsic** | Language-required operation with fixed semantics | Compile time + Runtime | No — hard-coded in `IntrinsicOp` enum |
| **Extern** | Embedder-provided Rust function | Runtime (VM execution) | Yes — via `ExternRegistry` |

### Design Philosophy

**Core intrinsics are minimal.** Only operations that require compiler
knowledge are intrinsics: operators (need type dispatch), `len()` (used in
for-loop lowering), and literal constructors. Functions like `is_defined()`
and `is_uint()` are prelude functions (convention) — regular Rill code that the
embedder optionally includes at compilation time.

**Externs are the embedding API.** Host applications register functions via
`ExternRegistry`. Functions can be registered globally (available without
`require`) or into a named namespace (requires a `require` declaration in
the script).

### Extern Registration

```rust
// All externs are namespaced — ExternDef carries namespace + name
registry.register(ExternDef::new("console", "print", print_fn))?;
registry.register(ExternDef::new("cbor", "decode", decode_fn))?;
registry.register(ExternDef::new("cbor", "encode", encode_fn))?;
registry.register(ExternDef::new("console", "log", log_fn))?;
```

Scripts declare their extern dependencies explicitly:

```rill
require cbor;
require console;

fn process(data) {
    let decoded = cbor::decode(data);
    console::log("processed");
    decoded
}
```

This makes extern dependencies visible to anyone reading the script —
critical for reusable source files where the reader needs to know what
the embedder must provide.

---

## Intrinsics

Intrinsics are operations with fixed semantics known to the compiler. Most lower
to `Instruction::Intrinsic { op: IntrinsicOp, args }`. Some expand to control flow.

### Lowering Table

| Syntax | Lowers To | Category |
|--------|-----------|----------|
| `x + y` | `Intrinsic(Add, [x, y])` | Single instruction |
| `x == y` | `Intrinsic(Eq, [x, y])` | Single instruction |
| `-x` | `Intrinsic(Neg, [x])` | Single instruction |
| `len(x)` | `Intrinsic(Len, [x])` | Single instruction |
| `append(arr, v)` | `Intrinsic(Append, [arr, v])` | Single instruction (mutating) |
| `[a, b, c]` | `Intrinsic(MakeArray, [a, b, c])` | Single instruction |
| `{k: v, ...}` | `Intrinsic(MakeMap, [k, v, ...])` | Single instruction |
| `start..end` | `Intrinsic(MakeSeq, [start, end, inclusive])` | Single instruction |
| `x != y` | `Not(Eq(x, y))` | Multi-instruction expansion |
| `x > y` | `Lt(y, x)` | Operand swap |
| `x <= y` | `Not(Lt(y, x))` | Multi-instruction expansion |
| `x >= y` | `Not(Lt(x, y))` | Multi-instruction expansion |
| `x && y` | `If(x, evaluate_y, false)` + Phi | Control flow (short-circuit) |
| `x \|\| y` | `If(x, true, evaluate_y)` + Phi | Control flow (short-circuit) |
| `if cond { a } else { b }` | `If` terminator + blocks + Phi | Control flow |
| `arr[i] = v` (lvalue) | `MakeAccessor` + `WriteAccessor` + `Reload` | SSA-visible mutation |
| `with x = arr[i]` | `MakeRef(arr, Some(i))` | Reference binding |
| `with x = y` | `MakeRef(y, None)` | Reference binding |
| `x = v` (ref-backed) | `WriteRef(ref_var, v)` + Copy + rebind | Write-back through reference |
| `x as UInt` | `Intrinsic(Convert(UInt, Unchecked), [x])` | Single instruction |

### Reflexive Comparison Operators

The comparison operators `!=`, `>`, `<=`, `>=` expand to combinations of the
primitive intrinsics `Eq`, `Lt`, and `Not`:

- `a != b` → `Not(Eq(a, b))`
- `a > b` → `Lt(b, a)` (operands swapped)
- `a <= b` → `Not(Lt(b, a))`
- `a >= b` → `Not(Lt(a, b))`

This reduces the intrinsic set to just `Eq` and `Lt` for comparisons, which is
sufficient because Rill uses `undefined` instead of IEEE-754 NaN. Without NaN's
special comparison semantics (where `NaN != NaN` is true), mathematical reflexivity
holds and these expansions are equivalent to dedicated operators.

### Prelude Functions (Not Intrinsics)

Functions like `is_defined()`, `is_uint()`, `default()`, etc. are **not
core intrinsics** — they are regular Rill functions, typically provided
in a `prelude.rill` file that scripts import. They
compile to identical IR as hand-written code:

```rill
fn is_defined(x) { if let _ = x { true } else { false } }
fn is_uint(x) { match x { UInt(_) => true, _ => false } }
fn default(value, fallback) { if let v = value { v } else { fallback } }
```

These produce the same Match + Phi control flow that a core intrinsic would. There is no performance penalty — the IR is identical. In bytecode,
they appear as internal functions in the function list.

---

## Extern Function System (ExternRegistry)

The `ExternRegistry` is the embedding API — how host applications register
Rust functions that Rill scripts can call. It follows Lua embedding patterns.

The standard registry is **empty**. All language-defined operations (`+`,
`len()`, etc.) are core intrinsics. Convenience functions (`is_uint()`,
`is_defined()`, etc.) are prelude functions (convention) (Rill source compiled
alongside user code). The registry exists for embedder-provided
functionality — namespaced
function groups (like `cbor::decode()`, `console::log()`).

All externs are namespaced — scripts use `require namespace;` to bring
them into scope. See the **Module System** section for details.

### Extern Metadata

```rust
struct ExternMeta {
    params: Vec<ParamSpec>,      // Parameter types and optionality
    returns: ReturnBehavior,     // Returns or Exits
    purity: Purity,              // Optimization potential + fallibility
}

enum ReturnBehavior {
    Returns(TypeSet),    // Normal return to caller
    Exits(TypeSet),      // Diverges — exits to driver
}

enum Purity {
    Impure,                                    // Side effects, always fallible
    Pure { fallible: bool },                   // No side effects, can't const-eval
    Const { eval: ConstEvalFn, fallible: bool }, // Can evaluate at compile time
}
```

### Purity and Fallibility

| Purity | Fallible | May Return Undefined? | Example |
|--------|----------|----------------------|---------|
| `Impure` | (always) | Yes - external factors | I/O, network |
| `Pure { fallible: false }` | No | No - always succeeds | Pure helper |
| `Const { fallible: true, .. }` | Yes | Yes - domain errors | Encoding |

The optimizer uses `purity.may_return_undefined()` for definedness analysis.
Intrinsics use `IntrinsicOp::is_fallible()` directly instead.

### Param Type Guards

When an extern declares `ParamSpec.type_sig` constraints, the compiler inserts
Match guards before the call during lowering:

```
// fn encode(data: Bytes) → result
// Lowered with guard:
Match(data_arg, [(Type(Bytes), call_bb)], default: skip_bb)

call_bb:
  result = Call("encode", [data_arg])   // data_arg proven Bytes
  Jump(join)

skip_bb:
  result = Undefined                     // type mismatch → skip call
  Jump(join)

join:
  Phi(result)
```

This means externs can trust their inputs — no internal type checking needed.
The guards integrate with the existing optimizer:

- **Type refinement**: narrows arg type in the call block (existing Match refinement)
- **Definedness**: arg is Defined in call block (existing Match scrutinee refinement)
- **Dead arm elimination**: collapses guard to Jump when type is statically known
- **Interprocedural analysis**: sees narrowed types at call sites automatically

### Type-Specialized Variants (Extern Monomorphism)

Externs can register type-specialized implementations that the compiler
selects at compile time based on argument types:

```rust
ExternDef::new("sqrt", sqrt_generic)
    .param("x", TypeSet::numeric())
    .returns(TypeSet::numeric())
    .variant(&[TypeSet::uint()], TypeSet::uint(), sqrt_uint)
    .variant(&[TypeSet::single(Float)], TypeSet::single(Float), sqrt_float)
```

When type analysis proves the argument is `{UInt}`, the compiler emits
`Call(sqrt_uint)` — the generic implementation and its internal type dispatch
are bypassed entirely. When types are unknown, the generic implementation
is used as a fallback.

Variant selection uses subset matching: `actual ⊆ spec` for each parameter.
The compiler resolves this at compile time in `compile_instruction` — zero
runtime overhead for the variant selection itself.

### Example Registration

```rust
let mut registry = ExternRegistry::new();

// All externs are namespaced — scripts use `require` to access
registry.register(
    ExternDef::new("runtime", "exit", exit_impl)
        .param_optional("code", TypeSet::uint())
        .exits(TypeSet::uint())
)?;

registry.register(
    ExternDef::new("cbor", "decode", cbor_decode_impl)
        .param("data", TypeSet::bytes())
        .returns(TypeSet::all())
        .pure()
)?;
registry.register(
    ExternDef::new("cbor", "encode", cbor_encode_impl)
        .param("value", TypeSet::all())
        .returns(TypeSet::bytes())
        .pure()
)?;
```

### Intrinsic vs Extern: Compilation

| Aspect | Intrinsic (`IntrinsicOp`) | Extern (`ExternRegistry`) |
|--------|--------------------------|---------------------------|
| **Registration** | Hard-coded in `IntrinsicOp` enum | `registry.register(ExternDef)` |
| **IR instruction** | `Instruction::Intrinsic { op, args }` | `Instruction::Call { function, args }` |
| **Const eval** | `eval_intrinsic_const()` in `const_eval.rs` | `Purity::Const { eval }` function pointer |
| **Runtime** | `exec_intrinsic()` in `compile/exec.rs` | Function pointer via `LinkMap` |
| **Type info** | `param_type()`, `result_type_refined()` | `ExternMeta.params`, `ExternMeta.returns` |
| **Link phase** | Not needed — compiled directly | Resolved via `LinkMap` at link time |

---

## Value Indexing

Values support indexing via methods (not a trait):

```rust
impl Value {
    /// Get value at index, returns None if not indexable or out of bounds
    pub fn get_at(&self, index: &Value) -> IndexResult { ... }

    /// Set value at index, returns false if not indexable/out of bounds
    pub fn set_at(&mut self, index: &Value, value: Value) -> bool { ... }
}

enum IndexResult {
    Value(Value),   // Existing value (cloned)
    Char(char),     // Text index - caller wraps in HeapVal
    Byte(u8),       // Bytes index - caller converts to UInt
    Undefined,      // Not found or not indexable
}
```

The VM wrapper handles heap allocation for results that need it:

```rust
impl VM {
    pub fn index_into(&mut self, container_idx: usize, index: &Value) -> Result<Value, ExecError> {
        let container = self.get(container_idx)?;
        match container.get_at(index) {
            IndexResult::Value(v) => Ok(v),
            IndexResult::Char(c) => Ok(Value::Text(HeapVal::new(c.to_string(), self.heap())?)),
            IndexResult::Byte(b) => Ok(Value::UInt(b as u64)),
            IndexResult::Undefined => Ok(Value::Undefined),
        }
    }
}
```

---

## Function Model

All functions are uniform - the host driver binds to entry points based on
function signatures in the compiled metadata.

### Function Metadata

```rust
struct FunctionMeta {
    name: String,
    params: Vec<ParamMeta>,
    return_type: TypeSignature,
}

struct ParamMeta {
    name: String,
    type_sig: TypeSignature,
    by_ref: bool,
}
```

### Host Driver Binding

The host driver compiles scripts and resolves function handles by name:

```rust
let (program, _) = compile(source, &externs).unwrap();

// Resolve once, call many times
let process = program.function("process").unwrap();
let validate = program.function("validate").unwrap();

// Execute with application data
let mut vm = VM::new();
vm.exec(&program)?;  // initialize file-scope globals (a no-op if there are none)
for record in records {
    vm.push(record.clone())?;
    validate.call(&mut vm, 1)?;
    vm.push(record)?;
    process.call(&mut vm, 1)?;
}
```

---

## Compiled Binary Format

Compiled output is CBOR-encoded for portability and self-description.

### Structure

```cbor
Tag(0xF1700) Module {
    version: uint,
    functions: [
        Tag(0xF1701) Function {
            name: text,
            params: [ParamMeta...],
            returns: TypeSignature | null,  // null = diverging
            code: Tag(0xF1702) [Instruction...],
        },
        ...
    ],
    constants: [ConstBinding...],
}
```

### Benefits

- **Self-describing**: Schema-flexible, extensible
- **Compact**: Efficient binary encoding
- **Extensible**: Custom tags for future features
- **Portable**: No platform-specific format dependencies

---

## Example: Embedding Rill

Rill is designed to be embedded in a host application. The host compiles
scripts, registers domain-specific externs, and calls script functions
with application data.

### Validation Pipeline

A typical pattern: the host loads a script containing validation functions
and runs them against incoming data.

```rill
// validation.rill
require time;

let MAX_AGE = 86400;

fn check_age(record) {
    if record.age > ::MAX_AGE {
        exit(1);  // reject — too old
    }
}

fn check_required_fields(record) {
    if !is_defined(record.id) {
        exit(2);  // reject — missing id
    }
    if !is_defined(record.payload) {
        exit(3);  // reject — missing payload
    }
}

fn transform(record) {
    record.processed = true;
    record.timestamp = time::now();
}
```

### Host Driver (Rust)

```rust
use rill::{Compiler, ExternDef, ExternRegistry, FileLoader, VM, Value};

// Register domain externs
let mut externs = ExternRegistry::new();
externs.register(ExternDef::new("runtime", "exit", exit_impl).exits(TypeSet::uint()))?;
externs.register(ExternDef::new("time", "now", time_now_impl))?;

// Compile once, execute many times
let loader = FileLoader::new("./scripts");
let mut compiler = Compiler::with_externs(externs, &loader);
compiler.add("main.rill");
let (program, _warnings) = compiler.build().unwrap();

// Resolve function handles for hot-path execution
let check_age = program.function("check_age").unwrap();
let check_fields = program.function("check_required_fields").unwrap();
let transform = program.function("transform").unwrap();

// Process incoming records
let mut vm = VM::new();
vm.exec(&program)?;  // initialize file-scope globals (a no-op if there are none)
for record in incoming_records {
    let data = record_to_value(&record);

    // Run validation — exit() returns Err with a disposition code
    vm.push(data.clone())?;
    match check_age.call(&mut vm, 1) {
        Ok(_) => {}  // passed
        Err(_) => { reject(record); continue; }
    }
    vm.push(data.clone())?;
    match check_fields.call(&mut vm, 1) {
        Ok(_) => {}
        Err(_) => { reject(record); continue; }
    }

    // Transform in-place
    vm.push(data)?;
    transform.call(&mut vm, 1).unwrap();
}
```

### The `exit()` Extern

The `exit(code)` extern is a diverging function — it exits the script
immediately and returns a disposition code to the host. This enables
filter/validation patterns without exceptions or error types.

```rust
registry.register(
    ExternDef::new("exit", extern_exit)
        .param("code", TypeSet::uint())
        .exits()
        .purity(Purity::Impure)
);

fn extern_exit(vm: &mut VM, argc: usize) -> Result<ExecResult, ExecError> {
    let code = if argc > 0 { vm.arg(0).clone() } else { Value::UInt(0) };
    Ok(ExecResult::Exit(code))
}
```

---

## Implementation Status

### Complete

- [x] Grammar specification (ABNF)
- [x] AST types
- [x] Parser (chumsky)
- [x] IR types and structures
- [x] IR lowering (AST → IR) — all expression, statement, and pattern types
- [x] VM core (stack, frames, slots)
- [x] Heap tracking with HeapVal (uses capacity() for accuracy)
- [x] Value types with Hash/Eq
- [x] Sequence type (SeqState: RangeUInt, RangeInt, ArraySlice with mutable flag)
- [x] Call convention with return slots
- [x] Reference binding via Slot::Ref (VM) + MakeRef/WriteRef (IR)
- [x] Extern registry and metadata system
- [x] Optimization passes (unified type-informed fixpoint loop):
  - [x] Constant folding, common subexpression elimination, copy propagation
  - [x] Dead code elimination, ref elision, coercion insertion and elision
  - [x] CFG simplification with jump threading and Phi simplification
  - [x] Type refinement, cast elision, algebraic simplification
  - [x] Condition folding, dead arm elimination (with `arms_cover_type`)
  - [x] Expression-level type guards (len, collect, append, cast, compound assignment, for-loop, range)
  - [x] Guard cache for duplicate type guard prevention
  - [x] Interprocedural analysis and function monomorphization
  - [x] Tail-call optimization
- [x] Diagnostics (W200/W201 definedness, W009 type mismatch, E300 cast errors)
- [x] SSA promotion (mem2reg: Assign/Read → VarId + Phi)
- [x] Public API: opaque `Program`, `compile()`, `Compiler` builder, `standard_externs()`
- [x] Source location utilities: `span_to_line_col()`, `LineCol`
- [x] For-loop pair binding: `for k, v in map { }`
- [x] Pattern lowering: Type, Map, ArrayRest with after patterns
- [x] TypeSet as u16 bitfield (Copy, const, zero heap)

### Pending

- [ ] Dead-store warnings for non-ref-backed loop variable mutations
- [ ] `if with` / match arm ref origin tracking (Phase 2)
- [ ] Dead write-back elimination (WriteRef where collection is never read after)
- [ ] Host sequence support (`SeqState::Host` variant)
- [x] Module system (`import` for source files, `require` for extern namespaces)
- [x] `ExternRegistry::register()` with self-describing `ExternDef`
- [x] Closure-threaded code execution (`src/compile/`)
- [ ] CBOR encode/decode integration
- [ ] Compiled binary format

---

## Design Decisions

### Why HeapVal instead of Rc directly?

Accurate heap tracking. Without HeapVal, we can't decrement usage when values are freed. HeapVal's Drop impl returns allocations to the shared heap counter.

### Why single stack for values and frames?

One `MAX_STACK_SIZE` check catches both value overflow and deep recursion. Simpler than maintaining two separate limits.

### Why Undefined instead of errors?

Inspired by SQL's `NULL` — a value that means "absent" and propagates through operations without crashing. Scripts can probe data structures without defensive checks, just as SQL queries can reference nullable columns without explicit null guards. Failed operations naturally propagate — no exceptions, no error types. This matches the duck-typed, schema-free nature of the language: any value can be probed for any field, and missing data is simply undefined rather than an error. Use `if let` to test presence, like SQL's `IS NOT NULL`.

### Why IndexMap for maps?

Preserves insertion order (important for serialization and deterministic output), provides O(1) lookup, and can be hashed for use as map keys (manual Hash impl iterates in order).

### Why Float wrapper?

Enables `Value` to implement `Hash` and `Eq`. NaN would break both. By enforcing no-NaN at construction, we get clean derived traits.

### Why return slot in Frame?

Avoids copying return values. Caller specifies where to write; callee writes directly. Essential for large values (maps, arrays) returned in loops.

### Why embed HeapRef inside Tracked<T>?

Drop::exit() takes no arguments, so deallocation tracking requires storing the heap reference somewhere accessible. By embedding HeapRef in the Rc'd allocation (Tracked<T>), HeapVal remains 8 bytes (one pointer). The cost is 8 extra bytes per allocation, not per HeapVal clone. This keeps Value at 16 bytes for better cache locality across the 65K-slot stack—a bigger win than saving 8 bytes per allocation.

### Why separate frame_stack instead of Frame slots?

FrameInfo is stored on a separate `Vec<FrameInfo>` stack rather than interleaved
with value slots. This keeps Slot at 16 bytes (no Frame variant needed), simplifies
slot addressing (params start at bp+0, not bp+1), and avoids boxing FrameInfo.
The frame stack is bounded by call depth (already limited by the value stack).

### Why three control flow primitives (If, Match, Jump)?

Each does exactly one thing:

- **If**: Boolean logic (true/false)
- **Match**: Type/value dispatch (BaseType, Literal, Array length)
- **Jump**: Unconditional

This separation enables clean lowering: type patterns → Match, conditions → If,
definedness checks → Match against defined types. No overloaded semantics.
The optimizer can reason about each independently.

### Why no `?` operator?

Undefined values propagate naturally through all operations: `undefined + 1` → undefined, `undefined.field` → undefined. This eliminates the need for explicit propagation operators. Use `if let`/`if with` when you need to handle the presence/absence case explicitly. This approach is simpler (fewer operators), more consistent (everything propagates), and aligns with the duck-typing philosophy.

### Why Purity as an enum (Impure/Pure/Const) instead of booleans?

It's a hierarchy: Const ⊂ Pure ⊂ Impure. Using an enum makes the hierarchy explicit and prevents invalid states (e.g., const but impure). Pattern matching is cleaner too. Additionally, `Const` carries a function pointer `ConstEvalFn` that enables compile-time evaluation - when all arguments are const, the compiler can call this function to compute the result during lowering.

### Why ReturnBehavior enum (Returns/Exits) instead of separate fields?

A function either returns to its caller or exits to the driver - never both. An enum prevents invalid states and makes the compiler's job easier: match on behavior, emit appropriate terminator.

### Why uniform function syntax (no special keywords for entry points)?

All functions use the same `fn` syntax. The host driver selects entry points
by name or convention, not by keyword. This keeps the language simple and
enables multiple use cases (validation, transforms, queries) without
domain-specific syntax.
- `exit()` as an extern rather than special syntax

### Why CBOR for compiled binary format?

CBOR is a good fit for the compiled format:

- Binary, compact, no text-parsing overhead
- Self-describing — schema-flexible, extensible via custom tags
- Natural representation of the language's value types
- No dependency on platform-specific formats
- Well-specified (RFC 8949), widely supported

### Why unified Definedness and Type in a single TypeSet?

Definedness and type are represented together in `TypeSet` — `Undefined` is a
type alongside `Bool`, `UInt`, etc. This unified representation enables a single
fixpoint loop where type-informed passes (coercion insertion, dead arm elimination)
and data-flow passes (const fold, copy prop, DCE) reinforce each other without
phase-ordering issues.

For example, `Add(Text, UInt)` starts as `MaybeDefined` (Add is fallible). The
coercion pass consults TypeAnalysis, sees the type mismatch, and replaces it with
`Instruction::Undefined`. On the next fixpoint iteration, constant folding
propagates the Undefined, dead arm elimination removes dead branches, and CFG
simplify cleans up. No separate definedness lattice needed — `TypeSet` tracks
both concerns in one powerset.

---

*Last updated: Reconciled with codebase — Convert unification, TCO, DCE, SSA, separate frame stack, updated pipeline.*
