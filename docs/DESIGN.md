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
- **Duck-typed values**: Nine base types covering scalars (Bool, UInt, Int, Float),
  strings (Text, Bytes), and collections (Array, Map, Sequence). No type annotations
  in source code — types are inferred by the optimizer.
- **Pattern matching**: Rich destructuring with type narrowing and reference binding.
- **Safe embedding**: Resource limits (stack, heap), no undefined behavior, host-provided builtins.
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
│   IR    │  SSA form, type sets, builtins
└────┬────┘
     │
     ▼
┌─────────┐
│   VM    │  Stack-based execution with heap tracking
└─────────┘
```

## Files

| File | Purpose |
|------|---------|
| `grammar.abnf` | Formal grammar specification |
| `types.rs` | Core type definitions (BaseType) shared by IR and exec |
| `ast.rs` | Abstract syntax tree types |
| `parser.rs` | Chumsky-based parser → AST |
| `ir.rs` | Intermediate representation (SSA) |
| `exec.rs` | Virtual machine and runtime values |
| `builtins.rs` | Builtin function registry and metadata |
| `stdlib_prelude.txt` | Auto-imported standard functions |
| `STDLIB.md` | Standard library documentation |

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

**Note on Sequence:** Sequence is a 9th internal type for lazy, single-pass values
(e.g., `0..10` creates a Sequence, not an Array). It is not user-visible as a type
name — users cannot pattern match on it. They interact with sequences through `for`
loops and `collect()`. The `..` operator is described as creating "a sequence", not
"a range object."

### Undefined Values

- **No null type**: Undefined values are tracked orthogonally
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
Value::Text(HeapVal<String>)
Value::Bytes(HeapVal<Vec<u8>>)
Value::Array(HeapVal<Vec<Value>>)
Value::Map(HeapVal<IndexMap<Value, Value>>)
Value::Sequence(HeapVal<SeqState>)
```

### SeqState

Internal state for lazy sequences:

```rust
pub enum SeqState {
    RangeUInt { current: u64, end: u64, inclusive: bool },
    RangeInt { current: i64, end: i64, inclusive: bool },
    ArraySlice { source: HeapVal<Vec<Value>>, start: usize, end: usize, mutable: bool },
}
```

- **RangeUInt/RangeInt**: Created by `0..10` / `0..=10`. O(1) memory.
- **ArraySlice**: Created by `..rest` patterns. Zero-copy reference to source array.
  The `mutable` flag follows the binding mode: `let` = false, `with` = true.
  Mutable slices allow for-loop write-back to the source array.

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
┌─────────────────┬────────┬────────┬─────┐
│ Frame(bp,ret)   │ param0 │ local0 │ ... │
└─────────────────┴────────┴────────┴─────┘
  bp+0              bp+1     bp+2
```

- **Slot 0**: Frame info (saved BP + return destination)
- **Offsets 1+**: Parameters, then locals
- **Single stack**: Values and call frames share one stack

### Slot Types

```rust
pub struct FrameInfo {
    pub bp: usize,
    pub return_slot: Option<usize>,
}

pub enum Slot {
    Val(Value),           // Actual value (16 bytes)
    Ref(usize),           // Reference to another slot (used by MakeRef with key: None)
    Frame(Box<FrameInfo>), // Frame info (boxed to keep Slot small)
    Uninit,               // Reserved but unassigned
}
// Slot is 16 bytes total (Value is largest at 16 bytes)
```

### Call Convention

```rust
// Caller: evaluate args, get absolute indices
let args = [arg0_idx, arg1_idx];
let return_slot = vm.bp() + dest_offset;

// Set up callee frame
vm.call(frame_size, Some(return_slot))?;

// Bind parameters
vm.bind_param(1, args[0], true);   // by ref
vm.bind_param(2, args[1], false);  // by val

// ... execute callee ...

// Callee returns - writes directly to caller's slot
vm.ret_val(result);

// Caller reads from local(dest_offset)
```

### Reference Binding (`with`)

Reference bindings create tracked aliases so that mutations flow back to the
source. The IR makes this explicit with two instructions: `MakeRef` creates the
reference and `WriteRef` performs write-back. The optimizer can see both and
reason about dead write-backs, forwarding, and alias relationships.

**Element references** (`with x = arr[i]`):

```
// IR:
v0 = MakeRef { base: arr, key: Some(i) }   // reads arr[i], records provenance
// bind "x" → v0

// x = 10  →  assignment to ref-backed variable
v1 = Const(10)
WriteRef { ref_var: v0, value: v1 }          // write-back: arr[i] = 10
// rebind "x" → v1 (for subsequent SSA reads)
```

At runtime, `MakeRef` with a key reads the element (like `Index`). `WriteRef`
resolves the ref_var back to its MakeRef to find (base, key) and emits a
`SetIndex` on the collection.

**Whole-value references** (`with x = y`):

```
// IR:
v0 = MakeRef { base: y, key: None }         // whole-value ref
// bind "x" → v0

// x = 10
v1 = Const(10)
WriteRef { ref_var: v0, value: v1 }          // write-back to y's slot
// rebind "x" → v1
```

At runtime, `MakeRef` without a key creates a `Slot::Ref` pointing to the
base's stack slot. `WriteRef` writes directly to the base slot, so the source
variable sees the new value.

**For-loop references** (`for x in arr { x += 1 }`):

```
// IR (body block):
v_ref = MakeRef { base: iter, key: Some(i_phi) }   // refs iter[i]
// bind "x" → v_ref

// x += 1  →  compound assignment
v_old = v_ref                            // current value
v_new = Intrinsic(Add, [v_old, 1])
WriteRef { ref_var: v_ref, value: v_new }   // write-back: iter[i] = v_new
// rebind "x" → v_new
```

The write-back is emitted at the point of assignment — correct even with
`break` and `continue` (no deferred write-back needed).

**Ref origin tracking:** The lowerer maintains a scoped `ref_origins` map
(`Identifier → RefOrigin { ref_var, base, key }`) alongside the normal scope
stack. When a ref-backed variable is assigned, the lowerer looks up its
`RefOrigin` and emits `WriteRef`.

**Compiler resolution:** At compile time, `build_ref_map()` collects all
`MakeRef` instructions into a `VarId → RefMeta { base_slot, key_slot }` map.
When compiling `WriteRef`, the compiler looks up the ref_var to find the
base and key slots and emits the appropriate closure:
- Element ref (key_slot is Some): SetIndex on the collection
- Whole-value ref (key_slot is None): set_local on the base slot

### Resource Limits

| Resource | Limit | Error |
|----------|-------|-------|
| Stack | 65,536 slots | `StackOverflow` |
| Heap | 16 MB (default) | `HeapOverflow` |

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
}

struct CompiledFunction {
    steps: Vec<Step>,           // all closures, flattened contiguously
    block_starts: Vec<usize>,   // block i starts at steps[block_starts[i]]
    entry: usize,               // index into block_starts
    frame_size: usize,          // VM slots to reserve (1 + locals)
}

type Step = Box<dyn Fn(&mut VM, &CompiledProgram) -> Result<Action, ExecError>>;

enum Action {
    Continue,                   // advance pc by 1
    NextBlock(usize),           // jump to block_starts[idx]
    Return(Option<Value>),      // return from function
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
    │     (VarIds → slot offsets, builtins → fn pointers)
    │
    ├─ 2. Compile each terminator to a Step closure
    │     (If/Match/Guard → NextBlock closures)
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
| `VarId(n)` | Stack slot offset `n + 1` |
| `FunctionRef("core::add")` | Native function pointer (Copy) |
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
| Cast/Widen target | Target always a constant | Target resolved at compile time, source-only dispatch |
| Index/MakeRef | Base type known | Type-specific indexing (no 5-way dispatch) |
| SetIndex/WriteRef | Base type known | Direct `set_array_elem` or `set_map_entry` |
| Match (single-arm) | From `if let` patterns | Inlined type/literal/length test |
| Match (multi-arm) | From `match` expressions | Pre-compiled predicate closures |
| Copy | Source provably Defined | Direct `.unwrap()` (no None check) |
| If condition | Provably Bool + Defined | Direct bool read (no null/type check) |
| Intrinsic args | All args provably Defined | `.unwrap()` then call (skip Option gate) |
| Non-Bool condition | Optimizer folds to Jump | `debug_assert!` in compiler |
| Identity Cast/Widen | Optimizer elides to Copy | `debug_assert!` in compiler |
| Guard definedness | Optimizer folds to Jump | `debug_assert!(MaybeDefined)` in compiler |

### Calling Convention

All function calls (user and builtin) use frame-based argument passing — no
intermediate `Vec` allocation:

- **User calls**: caller copies args slot-to-slot into callee's frame, executes
  callee body inline (same loop, no `execute_function` indirection)
- **Builtin calls**: frame set up with `call_with_args` (Lua-style: pre-pushed
  args adopted into frame via `rotate_right`). Builtins read args via `vm.arg(i)`
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
- **Zero allocation per call**: Args copied slot-to-slot, no Vec. Builtins
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
    Call { dest: usize, func: BuiltinFn, args: Vec<usize> },
    // ...
}
// Optimize StepKind sequences, then convert to closures
```

Candidates: copy-to-self elimination, dead store removal, constant + immediate
use fusion, jump threading.

### Future: Tail-Call Optimization

When a function's last action is calling another function (tail position), the
current frame can be reused instead of pushing a new one. This is an IR-level
transform:

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
    jump to entry;      // pc = block_starts[entry], no new frame
}
```

The flat pc-based architecture supports this naturally — TCO just rewrites
params and sets `pc` to the entry offset instead of recursing through
`execute_function`.

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
| **Intrinsic** | Language-defined operations with fixed semantics | `Add`, `Eq`, `Len`, `MakeArray` |
| **Extern Call** | Host-provided functions registered by embedder | `exit()`, `decode()`, `validate()` |

**Intrinsics** (`IntrinsicOp` enum) cover all language-defined operations:

- Arithmetic: `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Neg`
- Comparison: `Eq`, `Lt`
- Logical: `Not`, `And`, `Or`
- Bitwise: `BitAnd`, `BitOr`, `BitXor`, `BitNot`, `Shl`, `Shr`, `BitTest`, `BitSet`
- Collection: `Len`, `MakeArray`, `MakeMap`
- Sequence: `MakeSeq`, `ArraySeq`

Intrinsics emit `Instruction::Intrinsic { op, args }` in the IR. The compiler
knows their exact semantics, arity, result types, and fallibility — enabling
const folding, type refinement, and type mismatch diagnostics without any
registry lookup. `1 + 2` lowers to `Intrinsic(Add, [1, 2])` which the optimizer
folds to `3` using the inline const evaluator.

**Some intrinsics expand to control flow** instead of a single instruction:

- `is_uint(x)` → `Match(x, [(Type(UInt), BB_t)], BB_f)` + Phi → Bool
- `is_some(x)` → `Guard(x, BB_t, BB_f)` + Phi → Bool
- `x && y` → `If(x, evaluate_y, false)` + Phi (short-circuit)
- `x || y` → `If(x, true, evaluate_y)` + Phi (short-circuit)

**Extern calls** (`Instruction::Call`) are reserved for host-provided functions
registered via the `BuiltinRegistry`. The standard registry is empty — all
language-defined functions are intrinsics.

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

**Reference pattern** — `with` bindings use `MakeRef` instead of `Index`,
enabling write-back via `WriteRef`:

```
AST: with [a, b] = arr;

IR:
BB0:
    Match(arr, [(Array(2), BB_bind)], BB_fail)

BB_bind:
    %a = MakeRef(arr, Some(0))     // ref to arr[0]
    %b = MakeRef(arr, Some(1))     // ref to arr[1]
    Jump(BB_continue)

BB_fail:
    %a = Undefined
    %b = Undefined
    Jump(BB_continue)

BB_continue:
    // a and b are ref-backed: assignment emits WriteRef
    // e.g. a = 10  →  WriteRef(%a, 10) + rebind
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
| Logical | `!` `&&` `\|\|` | `Not`, `And`, `Or` | No |
| Bitwise | `&` `\|` `^` `~` `<<` `>>` | `BitAnd`, `BitOr`, `BitXor`, `BitNot`, `Shl`, `Shr` | No |
| Bit access | `@` | `BitTest` (read), `BitSet` (write) | Yes (OOB) |
| Collection | `len(x)` `[a,b]` `{k:v}` | `Len`, `MakeArray`, `MakeMap` | Yes / No / Yes |
| Sequence | `start..end` `..rest` | `MakeSeq`, `ArraySeq` | No |
| Coercion | (implicit) | `Widen` | Yes (overflow) |
| Cast | `x as UInt` | `Cast` | No |

Short-circuit operators (`&&`, `||`) lower to control flow (If + Phi), not a
single `Instruction::Intrinsic`. All other operators lower to
`Instruction::Intrinsic { op, args }`.

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

**Type cast operator (`as`):** Explicit infallible numeric cast. Distinct from
both type patterns (which test types) and the compiler-inserted `Widen` (which
is overflow-checked). `Cast` is user-requested and always succeeds for valid
numeric pairs:

| Source | `as UInt` | `as Int` | `as Float` |
|--------|-----------|----------|------------|
| UInt | identity | bit reinterpret | widen |
| Int | bit reinterpret | identity | widen |
| Float | — | — | identity |

- `as Bool`, `as Text`, etc. are **compile-time errors** (E300)
- Float → Int/UInt is not supported — use `floor()`, `round()`, `trunc()`
- Bool/Text/Bytes/Array/Map sources produce undefined at runtime
- Precedence: between unary and multiplicative — `-x as UInt` is `(-x) as UInt`

Lowering: `x as UInt` → `Intrinsic(Cast, [x, Const(1)])` where the target is
encoded as a UInt constant matching the `BaseType` discriminant (1=UInt, 2=Int,
3=Float). Follows the same encoding convention as `Widen`.

### Control Flow Primitives

Four fundamental control flow terminators, each with a single responsibility:

| Terminator | Purpose | Branches On |
|------------|---------|-------------|
| `If` | Boolean logic | true/false |
| `Match` | Type/structure dispatch | MatchPattern |
| `Guard` | Presence check | Defined/Undefined |
| `Jump` | Unconditional | - |

Plus terminators that exit the function:

| Terminator | Purpose |
|------------|---------|
| `Return` | Return value to caller |
| `Exit` | Hard exit to driver (from diverging builtins) |

```rust
pub enum Terminator {
    /// Unconditional jump
    Jump { target: BlockId },

    /// Branch on boolean condition
    If {
        condition: VarId,  // Must be Bool
        then_target: BlockId,
        else_target: BlockId,
    },

    /// Dispatch on type/structure (for type patterns)
    Match {
        value: VarId,
        arms: Vec<(MatchPattern, BlockId)>,
        default: BlockId,
    },

    /// Presence guard (for if let/if with pattern matching)
    /// In defined_target, value is known non-Undefined
    Guard {
        value: VarId,
        defined: BlockId,
        undefined: BlockId,
    },

    /// Return to caller
    Return { value: Option<VarId> },

    /// Hard exit to driver (never returns to caller)
    Exit { value: VarId },
}

/// Pattern for Match terminator arms
pub enum MatchPattern {
    Literal(Literal),      // Match specific value
    Type(BaseType),        // Match simple type (Bool, UInt, Int, Float, Text, Bytes, Map)
    Array(usize),          // Match array with exact length
    ArrayMin(usize),       // Match array with minimum length (rest patterns)
}
```

### The Guard Terminator

The `Guard` terminator checks if a value is defined (not Undefined) and branches:

1. Checks if value is defined (not Undefined)
2. In the defined block, value is known non-Undefined (type narrowing)
3. In the undefined block, handles the absence case

Used primarily for `if let`/`if with` pattern matching where presence determines execution:

```
// Source
if let x = maybe_value {
    use(x);
}

// Lowered IR
BB0:
    Guard(%maybe_value, BB_defined, BB_undefined)

BB_defined:
    %x = Copy(%maybe_value)  // x is known non-Undefined here
    // ... use(x) ...
    Drop(%x)
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
| `if let x = expr { }` | `Guard` + scoped binding |
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
allocates these variables - they simply don't exist outside the success block.
The `Drop` instruction marks when slots can be reclaimed.

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
    Drop(%a, %b)          // slots reclaimed
    Jump(BB_continue)

BB_else:
    // %a, %b never allocated here
    Jump(BB_continue)

BB_continue:
    // %a, %b not accessible
```

The `Drop` instruction serves two purposes:

1. **Slot reclamation**: The slot allocator can reuse these slots for later variables
2. **Scope enforcement**: Accessing dropped variables is a compile error

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
computation when assigning to invalid locations. The generated IR uses Guard
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
IR (lowered)
    │
    │  ── Phase 1: Fixpoint loop ──
    ▼
┌──────────────────────────────────────┐
│  ┌─────────────────────┐            │
│  │ Constant Folding    │            │
│  └──────────┬──────────┘            │
│             ▼                       │
│  ┌─────────────────────┐            │
│  │ Ref Elision         │            │
│  └──────────┬──────────┘            │
│             ▼                       │
│  ┌─────────────────────┐            │
│  │ Definedness Analysis│            │
│  │ + Diagnostics (1st) │            │
│  └──────────┬──────────┘            │
│             ▼                       │
│  ┌─────────────────────┐            │
│  │ Guard Elimination   │            │
│  │ + CFG Simplification│            │
│  └──────────┬──────────┘            │
│             │ ◄── repeat while      │
│             │     any pass changed  │
└─────────────┴────────────────────────┘
    │
    │  ── Phase 2: Type-informed ──
    ▼
┌─────────────────────┐
│ Type Refinement     │  Intrinsic-aware: Add(UInt,UInt) → {UInt}
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ Type Diagnostics    │  W009: type mismatch → always undefined
│                     │  W009: non-Bool If condition → always else
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ Coercion Insertion  │  Insert Widen for mixed-type arithmetic
│                     │  Replace incompatible ops with Undefined
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ Cast/Widen Elision  │  Identity Cast/Widen (src==target) → Copy
└──────────┬──────────┘
           ▼
┌─────────────────────┐
│ Condition Folding   │  Non-Bool If condition → Jump(else)
│                     │  Then-branch becomes dead → cleaned by Phase 1
└──────────┬──────────┘
           ▼
  Phase 1 fixpoint       (re-run if Phase 2 changed anything)
           ▼
┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
  Dead Code Elimination  (planned)
└ ─ ─ ─ ─ ┬ ─ ─ ─ ─ ┘
           ▼
IR (optimized)
```

### Fixpoint Iteration

The Phase 1 passes (const fold, ref elision, definedness, guard elim, CFG
simplify) run in a loop until no pass makes changes. This handles cascading
effects:

- Const fold may turn a Phi into a constant → definedness sees Defined
- Ref elision demotes read-only MakeRefs → exposes Copy/Index for const fold
- Guard elimination removes guards → CFG simplify removes dead blocks
- Dead block removal simplifies Phi nodes → new constant folding opportunities
- CFG simplify may remove WriteRefs → ref elision demotes more MakeRefs

Typically converges in 1-2 iterations. Diagnostics (E200/E201) are emitted
only on the first iteration, before guard elimination reshapes the flow.

### Two-Phase Definedness

The coercion insertion pass bridges type analysis into definedness:

1. **Phase 1** (coarse): Uses `is_fallible()` only. `Add(Text, UInt)` is
   conservatively `MaybeDefined` — Add *can* fail, but we don't know from
   definedness alone that it *always* fails for these types.

2. **After coercion insertion**: The coercion pass consults TypeAnalysis and
   emits explicit `Instruction::Undefined` for invalid type combinations.
   Re-running the Phase 1 fixpoint loop on the expanded IR sees these
   Undefined instructions → proves `Undefined` instead of `MaybeDefined`
   → eliminates guards → removes dead branches. No new infrastructure
   needed — just re-enter the existing loop.

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
| Chain shortening | `base` from `MakeRef(_, orig, None)` | `MakeRef(d, base, None)` → `MakeRef(d, orig, None)` |
| Element demotion | No `WriteRef` targets `dest` | `MakeRef(d, b, Some(k))` → `Index(d, b, k)` |
| Whole-value demotion | No `WriteRef` targets `dest` AND base not in `written_bases` | `MakeRef(d, b, None)` → `Copy(d, b)` |

**Written bases:** A base is "written" if any `WriteRef` in the function
modifies it — either through a whole-value write (`key: None`) or an element
write (`key: Some`, which mutates the collection). The pass follows `MakeRef`
chains to find the resolved base. If a sibling ref to the same base has a
`WriteRef`, the `Slot::Ref` alias must stay live so reads see the mutation.

**Example:** `with x = arr; with y = arr[0]; y = 10` — the `WriteRef` for `y`
writes to `arr`, so `arr` is in `written_bases`. If another ref aliases `arr`
via `MakeRef(_, arr, None)`, it cannot be demoted to `Copy` because it must
see the mutation to `arr[0]`.

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
| `Index { dest, .. }` | `MaybeDefined` (OOB possible) |
| `MakeRef { dest, .. }` | `MaybeDefined` (target may not exist) |
| `WriteRef { .. }` | no dest (side effect only) |
| `Intrinsic { op, .. }` infallible, all args Defined | `Defined` |
| `Intrinsic { op, .. }` fallible or args MaybeDefined | `MaybeDefined` |
| `Call { dest, function, .. }` | depends on `BuiltinMeta.purity` |
| `Phi { dest, sources }` | meet of all sources |

**Control flow refinement:**

At a `Guard` terminator:
- In the `defined` branch: guarded value becomes `Defined`
- In the `undefined` branch: guarded value becomes `Undefined`

**Why flow-sensitive analysis in SSA form?**

In SSA, each variable is assigned exactly once, so one might expect each variable
to have a single fixed definedness. However, Guards create contexts where we know
more than the variable's "intrinsic" definedness:

```
Block 0:
  v0 = param           // Intrinsic: MaybeDefined (caller might pass undefined)
  Guard v0 -> B1, B2

Block 1:               // After Guard, we KNOW v0 is Defined
  v1 = v0 + 1          // v1 is Defined (both operands are Defined)
  Guard v1 -> ...      // Can eliminate! (v1 is provably Defined)
```

Without flow-sensitivity, `v0` stays `MaybeDefined` everywhere, so `v1 = v0 + 1`
would compute as `MaybeDefined`, and we couldn't eliminate the second Guard.

With flow-sensitivity, after the Guard's defined branch, we track that `v0` is
`Defined` in that context, so `v1` becomes `Defined`, enabling Guard elimination.

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
| Control flow (`if` condition, `match` scrutinee) | **E200** error | **E201** warning |
| Data flow (intrinsic arg, index base/key, etc.) | **E200** warning | **E201** warning |

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

### Pass 3: Guard Elimination

**Goal:** Remove unnecessary Guard terminators.

**Rules:**

| Condition | Transformation |
|-----------|----------------|
| Guard value is `Defined` | Replace with `Jump { target: defined }` |
| Guard value is `Undefined` | Replace with `Jump { target: undefined }` |
| Guard value is `MaybeDefined` | Keep Guard (runtime check needed) |

After Guard elimination, run CFG simplification:
- Merge single-predecessor/single-successor blocks
- Remove unreachable blocks (no predecessors)
- Eliminate trivial jumps (jump to next block)

### Pass 4: Type Refinement

**Goal:** Narrow the `types` set in each variable's TypeSet.

This runs after Guard elimination so the CFG is simpler. Type refinement tracks
the possible concrete types (Bool, UInt, Int, etc.) at each program point.

**Lattice:** Powerset of `{Bool, UInt, Int, Float, Text, Bytes, Array, Map, Sequence}`

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

After Guard elimination and CFG simplification, new constant folding opportunities
may emerge. This pass runs the same constant folding logic as Pass 1 to clean up.

**Transformations:**

- Fold `Intrinsic` ops with const args: `Intrinsic(Add, [1, 2])` → `Const(3)`
- Fold `Call` to const builtins with const args
- Simplify `If` terminators: `If { condition: Const(true), .. }` → `Jump { target: then }`
- Replace variable references with `Const` instructions when value is known

### Pass 6: Dead Code Elimination (Planned)

**Goal:** Remove computations whose results are never used.

**Algorithm:**

1. Mark as "live":
   - Variables used in `Return`, `Exit` terminators
   - Variables used in `SetIndex` or `WriteRef` (side effects)
   - Variables used in impure `Call` arguments
   - Variables used in terminator conditions (`If`, `Guard`, `Match`)

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
src/ir/
├── opt/
│   ├── mod.rs             # Pipeline orchestration, optimize()
│   ├── const_fold.rs      # Passes 1 & 5: Constant folding
│   ├── ref_elision.rs     # Pass 1.5: Ref elision (MakeRef → Copy/Index)
│   ├── definedness.rs     # Pass 2: Definedness analysis + diagnostics
│   ├── guard_elim.rs      # Pass 3: Guard elimination + CFG simplification
│   ├── type_refinement.rs # Pass 4: Type refinement
│   ├── coercion.rs        # Pass 4.75: Coercion insertion (Widen + Undefined)
│   └── cast_elision.rs    # Pass 4.8: Identity Cast/Widen → Copy
```

### Fixed-Point Iteration

Some passes may enable further optimizations by others. The pipeline can iterate:

```rust
loop {
    let folded = fold_constants(&mut func, builtins, diagnostics);
    let refs = elide_refs(&mut func);
    let analysis = analyze_definedness(&func, Some(builtins));
    let guards = eliminate_guards(&mut func, &analysis);
    let blocks = simplify_cfg(&mut func);
    if folded + refs + guards + blocks == 0 { break; }
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
if is_some(arr[i] = v) { }

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

The language uses a consistent pattern: **default is by-reference**, use `let` to opt into by-value.
The `with` keyword can be used explicitly for by-reference (same as default) for symmetry and clarity.

| Context | by-ref (explicit) | by-ref (implicit) | by-value |
|---------|-------------------|-------------------|----------|
| Statement | `with x = expr` | — | `let x = expr` |
| Conditional | `if with x = expr { }` | — | `if let x = expr { }` |
| For loop (single) | `for with x in arr { }` | `for x in arr { }` | `for let x in arr { }` |
| For loop (pair) | `for with k, v in map { }` | `for k, v in map { }` | `for let k, v in map { }` |
| Match arm | `with pat => { }` | `pat => { }` | `let pat => { }` |
| Function param | `fn foo(with x)` | `fn foo(x)` | `fn foo(let x)` |

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

- **By-reference** (`with` or default): Variable refers to original location; mutations flow back
- **By-value** (`let`): Variable is a copy; mutations are local only

**Why allow explicit `with`?** Self-documenting code. When you write `fn process(with data)`,
it signals intent: "I will mutate this parameter." The explicit keyword is optional but encouraged
for clarity.

**IR-level reference architecture:**

Reference semantics are explicit in the IR via two instructions, making them
visible to the optimizer (no hidden aliasing):

| Instruction | Emitted When | Runtime Effect |
|-------------|-------------|----------------|
| `MakeRef { dest, base, key: Some(k) }` | `with x = arr[i]`, for-loop by-ref | Reads `base[key]` into `dest` (like Index) |
| `MakeRef { dest, base, key: None }` | `with x = y` | Creates `Slot::Ref` pointing to base's slot |
| `WriteRef { ref_var, value }` | Assignment to ref-backed variable | Element: `SetIndex(base, key, value)`. Whole-value: slot write |

The lowerer tracks ref origins in a scoped `HashMap<Identifier, RefOrigin>`
(managed alongside the scope stack). When a ref-backed variable is assigned
(`x = 10`), the lowerer:
1. Looks up `x` in `ref_origins` → finds `RefOrigin { ref_var, base, key }`
2. Emits `WriteRef { ref_var, value: v_new }`
3. Creates a new SSA VarId and rebinds `x` (normal SSA behaviour)

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

- `..args` - by-reference (default)
- `let ..args` - by-value (copy)
- `with ..args` - explicit by-reference

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

### Prelude Functions

Auto-imported functions available without qualification.

**Always inlined** (expansion simpler than a call):

| Function | Returns | Inlines To |
|----------|---------|------------|
| `is_uint(v)`, `is_int(v)`, ... | `Bool` | `Match` + Phi |
| `is_some(v)` | `Bool` | `Guard` + Phi |

**Regular functions** (Const purity, may be inlined):

| Function | Returns | Purpose |
|----------|---------|---------|
| `to_uint(v)`, `to_int(v)`, ... | Value or Undefined | Type conversion |
| `len(v)` | `UInt` or Undefined | Collection/sequence length |
| `concat(a, b)` | Collection or Undefined | Concatenate arrays, text, bytes |
| `collect(seq)` | Array | Materialize a sequence into an Array |
| `sub_slice(coll, start, end)` | Collection | Extract sub-collection [start, end) |

**Internal functions** (compiler-generated for lowering literals):

| Function | Returns | Used For |
|----------|---------|----------|
| `make_array()` | Array | `[a, b, c]` literals |
| `make_map()` | Map | `{k: v}` literals |
| `push(coll, elem)` | Collection | Building collection literals |
| `insert(map, k, v)` | Map | Building map literals |

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

Host builtins can return sequences for lazy data streams (e.g., iterating over
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

- Type mismatch in builtin
- Division by zero
- Arithmetic overflow/underflow
- Out of bounds index
- Failed map lookup
- Invalid type conversion

Scripts handle with:

```rust
if is_some(x) { use(x) }     // Existence check
if let v = to_uint(x) { }    // Conditional binding
let y = x;                   // Undefined propagates through operations
```

---

## Module System

### Import Syntax

Imports introduce namespaces using dotted paths (stdlib) or string paths (files):

```rill
// Standard library - dotted path, last segment becomes namespace
import std.cbor;                     // → namespace `cbor`
import std.encoding.base64;          // → namespace `base64`
import std.time as t;                // → namespace `t` (explicit alias)

// Local files - string path, filename becomes namespace
import "../common/validation.rill";  // → namespace `validation`
import "./helpers.rill" as h;        // → namespace `h` (explicit alias)
```

### Namespace Qualification

All namespace-qualified access uses `::` separator at call sites:

```rill
// Function calls
cbor::decode(bytes)             // Function from imported module
base64::encode(data)            // Function from imported module
console::log("hello")           // Embedding-provided namespace

// Constant access
http::STATUS_OK                 // Constant from imported module
config::MAX_TIMEOUT             // Constant from imported module

// Unqualified names
len(arr)                        // Prelude function (no namespace)
my_function(x)                  // Local function (no namespace)
```

**Key distinction:**
- Import paths use `.` (dotted notation): `import std.cbor.utils;`
- Call sites use `::` (qualification): `utils::decode()`

### Visibility

Function and constant visibility is **structural**, not declarative — there is no
`pub` keyword.

| Declared in | Visibility | Callable by embedder? | DCE eligible? |
|------------|------------|----------------------|---------------|
| Root file | Public | Yes (`FunctionHandle`) | No — always kept |
| Imported file | Private | No | Yes — removed if unused |

The root file is the file passed to `compile()`. Everything declared directly in
it is a potential entry point for the embedder. Imported files provide helper
functions and constants that are implementation details.

```rust
// root.rill — all functions here are public
import "./helpers.rill";

fn process(data) { ... }            // public — embedder can call this
fn validate(record) { ... }         // public — embedder can call this
const MAX_TIMEOUT = 30000;          // public — visible to embedder

// helpers.rill — all functions here are private
fn compute_checksum(data) { ... }   // private — only callable from root
fn internal_helper() { ... }        // private — if unused, eliminated by DCE
```

This means there is no such thing as an "unused function" in the root file —
every root function is a potential entry point. Imported functions that are
never referenced are dead code and can be eliminated during compilation.

### Reserved Namespaces

| Namespace | Purpose | User-definable? |
|-----------|---------|-----------------|
| (embedding) | Host-provided (`console`, `file`, etc.) | Registered by host |
| (user) | Imported modules, local definitions | Yes |

No `core::` namespace is reserved — all language-defined operations are intrinsics
recognized by the compiler during lowering, not namespace-qualified functions.

### Name Lookup Order

When resolving an unqualified function call, the lowerer searches in order:

1. **Intrinsics** — `len()`, `is_some()`, `is_uint()`, etc. (checked first via `try_lower_intrinsic`)
2. **Registry** — host-provided extern functions (via `BuiltinRegistry`)
3. **User functions** — defined in current module or imported

For const declarations, `intrinsic_by_name()` maps function names to `IntrinsicOp`
before falling through to the registry.

### Embedding-Provided Namespaces

Embedding applications register namespaces with functions:

```rust
// Host application (Rust)
context.register_namespace("console", vec![
    ("log", console_log_impl),
    ("error", console_error_impl),
]);
```

```rill
// Rill code - no import needed
console::log("Hello, world!")
```

### Prelude (Planned)

A future prelude will provide standard utility functions that are automatically
available without imports. These are regular user-defined functions, not intrinsics:

```rill
// Planned prelude functions (user-definable, identical IR to hand-written):
fn is_some(x) { if let _ = x { true } else { false } }
fn is_uint(x) { match x { UInt(_) => true, _ => false } }
fn is_int(x) { match x { Int(_) => true, _ => false } }
// ... etc for all types
fn default(value, fallback) { if let v = value { v } else { fallback } }
```

The only compiler intrinsic that is user-callable by name is `len()`, which the
compiler recognizes in `try_lower_intrinsic` and emits as `Intrinsic(Len, [x])`.
This is necessary because `len()` is used internally by for-loop lowering and
pattern matching.

---

## Builtins and Intrinsics

Rill distinguishes between two types of "built-in" functionality:

| Concept | Definition | When Evaluated | Registered? |
|---------|------------|----------------|-------------|
| **Intrinsic** | Language-defined operation with fixed semantics | Compile time + Runtime | No — hard-coded in `IntrinsicOp` enum |
| **Extern** | Host-provided Rust function | Runtime (VM execution) | Yes — via `BuiltinRegistry` |

### Design Philosophy

**Intrinsics are minimal.** Only operations that require compiler knowledge are
intrinsics: operators (need type dispatch), `len()` (used in for-loop lowering),
and literal constructors. Functions like `is_some()` and `is_uint()` are
user-definable — they compile to identical IR via normal `match`/`if let` syntax.

**Externs are the embedding API.** Host applications register functions that scripts
can call by name. The standard registry is empty.

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
| `arr[i] = v` (lvalue) | `Index` + `Guard` + `SetIndex` + Phi | Control flow |
| `with x = arr[i]` | `MakeRef(arr, Some(i))` | Reference binding |
| `with x = y` | `MakeRef(y, None)` | Reference binding |
| `x = v` (ref-backed) | `WriteRef(ref_var, v)` + Copy + rebind | Write-back through reference |
| `x as UInt` | `Intrinsic(Cast, [x, Const(1)])` | Single instruction |

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

### User-Definable Utility Functions

Functions like `is_some()`, `is_uint()`, `default()`, etc. are **not intrinsics**.
Users define them as regular functions — they compile to identical IR:

```rill
fn is_some(x) { if let _ = x { true } else { false } }
fn is_uint(x) { match x { UInt(_) => true, _ => false } }
fn default(value, fallback) { if let v = value { v } else { fallback } }
```

These produce the same Guard/Match + Phi control flow that a compiler intrinsic
would. There is no performance penalty — the IR is identical.

A future prelude will provide standard definitions of common utility functions.

---

## Extern Function System (BuiltinRegistry)

The `BuiltinRegistry` is the embedding API — how host applications register
functions that Rill scripts can call by name. It follows Lua embedding patterns.

The standard registry is **empty**. All language-defined operations (`+`, `len()`,
`is_uint()`, etc.) are intrinsics. The registry exists purely for host-provided
functionality like `exit()`, `encode()`, or domain-specific operations.

### Builtin Metadata

```rust
struct BuiltinMeta {
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

### Example Registration

```rust
let mut registry = BuiltinRegistry::new();

// Exit — diverges, implicitly impure
registry.register(
    BuiltinDef::new("exit", exit_impl)
        .param_optional("code", TypeSet::uint())
        .exits(TypeSet::uint())
);

// Host-provided encoding
registry.register(
    BuiltinDef::new("encode", cbor_encode_impl)
        .param("value", TypeSet::all())
        .returns(TypeSet::bytes())
        .pure()
);
```

### Intrinsic vs Extern: Compilation

| Aspect | Intrinsic (`IntrinsicOp`) | Extern (`BuiltinRegistry`) |
|--------|--------------------------|---------------------------|
| **Registration** | Hard-coded in `IntrinsicOp` enum | `registry.register(BuiltinDef)` |
| **IR instruction** | `Instruction::Intrinsic { op, args }` | `Instruction::Call { function, args }` |
| **Const eval** | `eval_intrinsic_const()` in `const_eval.rs` | `Purity::Const { eval }` function pointer |
| **Runtime** | `exec_intrinsic()` in `compile.rs` | Function pointer via `LinkMap` |
| **Type info** | `param_type()`, `result_type_refined()` | `BuiltinMeta.params`, `BuiltinMeta.returns` |
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
let (program, _) = compile(source, &builtins).unwrap();

// Resolve once, call many times
let process = program.function("process").unwrap();
let validate = program.function("validate").unwrap();

// Execute with application data
let mut vm = VM::new();
for record in records {
    validate.call(&mut vm, &[record.clone()])?;
    process.call(&mut vm, &[record])?;
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
scripts, registers domain-specific builtins, and calls script functions
with application data.

### Validation Pipeline

A typical pattern: the host loads a script containing validation functions
and runs them against incoming data.

```
// validation.rill
const MAX_AGE = 86400;

fn check_age(record) {
    if record.age > MAX_AGE {
        exit(1);  // reject — too old
    }
}

fn check_required_fields(record) {
    if !is_some(record.id) {
        exit(2);  // reject — missing id
    }
    if !is_some(record.payload) {
        exit(3);  // reject — missing payload
    }
}

fn transform(with record) {
    record.processed = true;
    record.timestamp = time::now();
}
```

### Host Driver (Rust)

```rust
use rill::{compile, standard_builtins, VM, Value};

// Register domain builtins
let mut builtins = standard_builtins();
builtins.register(/* time::now, logging, etc. */);

// Compile once, execute many times
let (program, _warnings) = compile(source, &builtins).unwrap();

// Resolve function handles for hot-path execution
let check_age = program.function("check_age").unwrap();
let check_fields = program.function("check_required_fields").unwrap();
let transform = program.function("transform").unwrap();

// Process incoming records
let mut vm = VM::new();
for record in incoming_records {
    let data = record_to_value(&record);

    // Run validation — exit() returns Err with a disposition code
    match check_age.call(&mut vm, &[data.clone()]) {
        Ok(_) => {}  // passed
        Err(_) => { reject(record); continue; }
    }
    match check_fields.call(&mut vm, &[data.clone()]) {
        Ok(_) => {}
        Err(_) => { reject(record); continue; }
    }

    // Transform in-place
    transform.call(&mut vm, &[data]).unwrap();
}
```

### The `exit()` Builtin

The `exit(code)` builtin is a diverging function — it exits the script
immediately and returns a disposition code to the host. This enables
filter/validation patterns without exceptions or error types.

```rust
registry.register(
    BuiltinDef::new("exit", builtin_exit)
        .param("code", TypeSet::uint())
        .exits()
        .purity(Purity::Impure)
);

fn builtin_exit(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let code = args.first().cloned().unwrap_or(Value::UInt(0));
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
- [x] Builtin registry and metadata system
- [x] Optimization passes:
  - [x] Constant folding (early + cleanup)
  - [x] Definedness analysis
  - [x] Diagnostics (warnings/errors from definedness)
  - [x] Guard elimination
  - [x] CFG simplification
  - [x] Type refinement
- [x] Public API: opaque `Program`, `compile()`, `standard_builtins()`
- [x] Source location utilities: `span_to_line_col()`, `LineCol`
- [x] For-loop pair binding: `for k, v in map { }`
- [x] Pattern lowering: Type, Map, ArrayRest with after patterns
- [x] TypeSet as u16 bitfield (Copy, const, zero heap)

### Pending

- [ ] Instruction execution (IR interpreter or VM codegen)
- [ ] Builtin implementations: `core::make_seq`, `core::seq_next`,
      `core::array_seq`, `core::collect`
- [ ] For-loop type dispatch (Match on iterable type for unknown types)
- [ ] For-loop sequence path (seq_next-based loop for Sequence type)
- [ ] Dead-store warnings for non-ref-backed loop variable mutations
- [ ] `if with` / match arm ref origin tracking (Phase 2)
- [ ] Dead write-back elimination (WriteRef where collection is never read after)
- [ ] Host sequence support (`SeqState::Host` variant)
- [ ] Standard library modules (std.time, std.cbor, std.encoding, std.parsing)
- [ ] Module/import resolution system
- [ ] CBOR encode/decode integration
- [ ] Compiled binary format
- [ ] Dead code elimination pass

---

## Design Decisions

### Why HeapVal instead of Rc directly?

Accurate heap tracking. Without HeapVal, we can't decrement usage when values are freed. HeapVal's Drop impl returns allocations to the shared heap counter.

### Why single stack for values and frames?

One `MAX_STACK_SIZE` check catches both value overflow and deep recursion. Simpler than maintaining two separate limits.

### Why Undefined instead of errors?

Duck typing philosophy. Scripts can probe values without try/catch. Failed operations naturally propagate — no exceptions, no error types. This matches the duck-typed, schema-free nature of the language: any value can be probed for any field, and missing data is simply undefined rather than an error.

### Why IndexMap for maps?

Preserves insertion order (important for serialization and deterministic output), provides O(1) lookup, and can be hashed for use as map keys (manual Hash impl iterates in order).

### Why Float wrapper?

Enables `Value` to implement `Hash` and `Eq`. NaN would break both. By enforcing no-NaN at construction, we get clean derived traits.

### Why return slot in Frame?

Avoids copying return values. Caller specifies where to write; callee writes directly. Essential for large values (maps, arrays) returned in loops.

### Why embed HeapRef inside Tracked<T>?

Drop::exit() takes no arguments, so deallocation tracking requires storing the heap reference somewhere accessible. By embedding HeapRef in the Rc'd allocation (Tracked<T>), HeapVal remains 8 bytes (one pointer). The cost is 8 extra bytes per allocation, not per HeapVal clone. This keeps Value at 16 bytes for better cache locality across the 65K-slot stack—a bigger win than saving 8 bytes per allocation.

### Why Box<FrameInfo> instead of inline?

FrameInfo has two `usize` fields (16 bytes). Inlining would make Slot 24 bytes, wasting 8 bytes on every Val/Ref/Uninit slot. Boxing adds one allocation per call frame, but frames are rare (one per function) while value slots are common. The space savings on 65K slots far outweigh the allocation cost.

### Why not track FrameInfo allocations?

Frame allocations are bounded by stack depth (already limited), tiny (16 bytes), and short-lived (freed on return). The heap limit is conceptually for script data, not VM bookkeeping. Stack overflow already catches runaway recursion.

### Why four control flow primitives (If, Match, Guard, Jump)?

Each does exactly one thing:

- **If**: Boolean logic (true/false)
- **Match**: Type dispatch (BaseType)
- **Guard**: Presence check (Defined/Undefined)
- **Jump**: Unconditional

This separation enables clean lowering: `if let` → Guard, type patterns → Match, conditions → If. No overloaded semantics. The optimizer can reason about each independently.

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
- `exit()` as a builtin rather than special syntax

### Why CBOR for compiled binary format?

CBOR is a good fit for the compiled format:

- Binary, compact, no text-parsing overhead
- Self-describing — schema-flexible, extensible via custom tags
- Natural representation of the language's value types
- No dependency on platform-specific formats
- Well-specified (RFC 8949), widely supported

### Why separate Definedness and Type analysis passes?

These are independent concerns with different lattices:
- **Definedness**: 3-value lattice (`Defined` / `MaybeDefined` / `Undefined`)
- **Type**: powerset of `{Bool, UInt, Int, Float, Text, Bytes, Array, Map, Sequence}`

A value can be "definitely defined" without knowing its type, and vice versa.

Splitting the analyses provides:

1. **Earlier CFG simplification**: Definedness analysis removes Guards, simplifying
   the control flow graph before type analysis runs
2. **Cleaner lattices**: Each pass has a simple, well-defined lattice
3. **Better diagnostics**: Definedness errors ("value is undefined") are distinct
   from type errors ("expected UInt, got Text")
4. **Efficiency**: Type refinement runs on a simpler CFG with fewer blocks

However, the two analyses are not fully independent — type analysis can *inform*
definedness. For example, `Add(Text, UInt)` is conservatively `MaybeDefined` in
the coarse pass (Add is fallible), but type analysis proves the result is always
undefined (Text is not numeric).

The coercion insertion pass (planned) bridges this gap: it consults type analysis
and generates explicit `Instruction::Undefined` for invalid type combinations.
Running definedness analysis again on the expanded IR tightens `MaybeDefined` to
`Undefined` where types prove the operation cannot succeed — no unified lattice
needed.

---

*Last updated: IntrinsicOp refactor, two-phase definedness, type-informed optimization pipeline.*
