# Rill Language Design Document

This document captures the design of the Rill language.

## Overview

Rill is a general-purpose embeddable scripting language with first-class
support for CBOR data. While originally created for DTN bundle processing, it is
a standalone language suitable for any domain requiring CBOR manipulation, data
transformation, or policy enforcement.

**Core features:**

- **CBOR-native types**: Full support for the CBOR data model (maps, arrays, bytes, etc.)
- **Pattern matching**: Rich destructuring with type narrowing
- **Reference semantics**: `with` bindings for in-place mutation
- **Embeddable**: Lua-style builtin registration for host integration
- **Safe**: Resource limits (stack, heap), no undefined behavior

**Use cases:**

- CBOR document validation and manipulation
- Data transformation and policy enforcement
- Embedded scripting for applications
- Configuration and rule processing
- Domain-specific applications (e.g., DTN bundle filtering)

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

| Type | Rust Representation | CBOR Major Type |
|------|---------------------|-----------------|
| `Bool` | `bool` | 7 (simple) |
| `UInt` | `u64` | 0 |
| `Int` | `i64` | 1 |
| `Float` | `Float` wrapper | 7 (float) |
| `Text` | `HeapVal<String>` | 3 |
| `Bytes` | `HeapVal<Vec<u8>>` | 2 |
| `Array` | `HeapVal<Vec<Value>>` | 4 |
| `Map` | `HeapVal<IndexMap<Value, Value>>` | 5 |
| `Sequence` | `HeapVal<SeqState>` | — (internal) |

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
    Ref(usize),           // Reference to another slot
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

```rust
// Source: with x = arr[0];
// IR: creates Ref slot pointing to arr[0]

vm.set_local_ref(x_offset, arr_element_idx);

// Reading x: follows Ref chain
let val = vm.local(x_offset);  // resolves Ref

// Writing x: resolves Ref, writes to target
vm.set_local(x_offset, new_val);  // mutates arr[0]
```

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
| `Literal::UInt(42)` | Cloned into the closure |
| `BlockId` | Index into `block_starts` |

### Execution Loop

The executor is a single flat loop with a program counter:

```rust
fn execute_function(program, vm, func_idx, args) -> Action {
    let func = &program.functions[func_idx];
    vm.call(func.frame_size, None)?;
    bind_params(vm, args);

    let mut pc = func.block_starts[func.entry];

    loop {
        match (func.steps[pc])(vm, program)? {
            Action::Continue    => pc += 1,
            Action::NextBlock(i) => pc = func.block_starts[i],
            Action::Return(val) => { vm.ret(); return Return(val); }
            Action::Exit(val)   => { vm.ret(); return Exit(val); }
        }
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
- **User function calls are recursive**: `execute_function` calls itself, bounded
  by the VM's `MAX_STACK_SIZE` (65K slots, ~3000-6000 call levels).
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
- **Reference tracking**: Compile-time alias analysis

### Two Categories of Operations

| Category | Description | Examples |
|----------|-------------|----------|
| **Intrinsic** | Short-circuit operators (require control flow) | `And`, `Or` |
| **Function** | Everything else (may be inlined or called) | `core::add`, `len()`, `exit()` |

**Intrinsics** are minimal - only `And` (`&&`) and `Or` (`||`) which require
short-circuit evaluation (control flow to skip the second operand).

**Core builtins** (`core::*`) implement all other operators with `Purity::Const`:

- Arithmetic: `core::add`, `core::sub`, `core::mul`, `core::div`, `core::mod`, `core::neg`
- Comparison: `core::eq`, `core::lt`
- Logical: `core::not`
- Bitwise: `core::bit_and`, `core::bit_or`, `core::bit_xor`, `core::bit_not`, `core::shl`, `core::shr`, `core::bit_test`, `core::bit_set`

These const builtins enable compile-time folding: `1 + 2` lowers to `Call("core::add", [1, 2])`
which the optimizer folds to `3` using the const evaluator.

**Other functions** include user-defined, prelude, and host-provided:

- *Prelude*: `len()`, `concat()`, `to_uint()`, `is_uint()`, `is_some()`, etc.
- *Host*: `exit()`, `decode()`, `validate()`

Functions may be inlined by the optimizer. Some prelude functions always inline
because the expansion is simpler than a call:

- `is_uint(x)` → `Match(x, [(Type(UInt), BB_t)], BB_f)` + Phi → Bool
- `is_some(x)` → `Guard(x, BB_t, BB_f)` + Phi → Bool

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

### Intrinsic Operations

Intrinsics are minimal - only the short-circuit boolean operators that require
control flow to avoid evaluating the second operand:

| Intrinsic | Syntax | Semantics |
|-----------|--------|-----------|
| `And` | `&&` | Short-circuit: `false && x` → `false` without evaluating `x` |
| `Or` | `\|\|` | Short-circuit: `true \|\| x` → `true` without evaluating `x` |

All other operators are implemented as **core builtins** with `Purity::Const`:

| Category | Syntax | Builtin |
|----------|--------|---------|
| Comparison | `==` `<` | `core::eq`, `core::lt` |
| Comparison | `!=` `>` `<=` `>=` | *Intrinsics* → expand to `eq`/`lt`/`not` |
| Arithmetic | `+` `-` `*` `/` `%` `-x` | `core::add`, `core::sub`, `core::mul`, `core::div`, `core::mod`, `core::neg` |
| Logical | `!` | `core::not` |
| Bitwise | `&` `\|` `^` `~` `<<` `>>` | `core::bit_and`, `core::bit_or`, `core::bit_xor`, `core::bit_not`, `core::shl`, `core::shr` |
| Bit access | `@` | `core::bit_test` (read), `core::bit_set` (write) |

Other builtins with appropriate purity annotations:

- Collection: `len()`, `concat()`, `push()`, `insert()`, `sub_slice()`, `collect()`
- Sequences: created by `..` operator; `core::make_seq`, `core::seq_next`, `core::array_seq`
- Type conversion: `to_uint()`, `to_int()`, `to_float()`, `to_text()`
- Type checking: `is_uint()`, `is_some()`, etc.

The runtime executes intrinsics and builtins the same way. The distinction exists
because intrinsics require control flow, while builtins are simple function calls
that can be const-folded when arguments are known at compile time.

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

The `@` operator lowers to builtin calls:

- Read: `core::bit_test(value, bit)` → Bool or undefined
- Write: `core::bit_set(value, bit, bool)` → value or undefined

---

## Optimization Pipeline

The IR goes through a series of optimization passes after lowering. The passes are
ordered to maximize effectiveness: earlier passes simplify the CFG, enabling later
passes to do more work.

### Pass Overview

```
IR (lowered)
    │
    ▼
┌─────────────────────┐
│ Constant Folding    │  Fold obvious compile-time constants early
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ Definedness Analysis │  Compute which values are provably defined
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ Diagnostics         │  Emit warnings/errors based on definedness
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ Guard Elimination   │  Remove Guards for provably-defined values
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ CFG Simplification  │  Merge blocks, remove unreachable code
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ Type Refinement     │  Narrow TypeSets based on control flow
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ Constant Folding    │  Cleanup pass after CFG changes
└──────────┬──────────┘
           │
           ▼
┌─────────────────────┐
│ Dead Code Elimination│  Remove unused computations (planned)
└──────────┬──────────┘
           │
           ▼
IR (optimized)
```

### Pass 1: Early Constant Folding

**Goal:** Fold obvious compile-time constants before analysis.

This pass runs first to simplify the IR before analysis passes. It evaluates
const builtin calls (like `core::add(1, 2)`) when all arguments are literal
constants, replacing them with `Const` instructions.

**Transformations:**

- `Call core::add(Const(1), Const(2))` → `Const(3)`
- `Call core::eq(Const(true), Const(false))` → `Const(false)`
- Constant If conditions → `Jump` to appropriate target

Running constant folding early simplifies the CFG for subsequent analysis passes.

### Pass 2: Definedness Analysis

**Goal:** Determine which variables are provably defined (not Undefined).

This analysis is orthogonal to type analysis - a value can be "definitely defined"
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
| `Call { dest, function, .. }` | depends on function return signature |
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

### Pass 2.5: Diagnostics

**Goal:** Emit warnings and errors based on definedness analysis.

Uses the computed definedness lattice to detect problematic code patterns:

**Warnings (code compiles, but may have issues):**

| Condition | Warning |
|-----------|---------|
| Assignment to `MaybeDefined` lvalue without checking result | "unchecked assignment to potentially-undefined location" |
| Using `MaybeDefined` value where `Defined` expected (without guard) | "value may be undefined; consider using `if let`" |

**Errors (code does not compile):**

| Condition | Error |
|-----------|-------|
| Using `Undefined` value where `Defined` required | "value is definitely undefined" |
| Assignment to `Undefined` lvalue | "assignment to definitely-undefined location has no effect" |
| Calling function with `Undefined` argument for non-optional parameter | "passing undefined to non-optional parameter" |

**Rationale:** The definedness lattice is computed anyway for Guard elimination.
Using it for diagnostics catches bugs early (compile-time vs runtime) without
additional analysis cost.

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

**Lattice:** Powerset of `{Bool, UInt, Int, Float, Text, Bytes, Array, Map}`

Meet = intersection (narrowing), Join = union (at Phi nodes)

**Transfer rules:**

| Instruction | Result Types |
|-------------|--------------|
| `Const { value: Literal::Bool(_), .. }` | `{Bool}` |
| `Const { value: Literal::UInt(_), .. }` | `{UInt}` |
| `Call { function: "core::add", .. }` | `{UInt, Int, Float}` |
| `Call { function: "core::eq", .. }` | `{Bool}` |
| `Call { function: "len", .. }` | `{UInt}` |
| `Index { .. }` | depends on base type |
| `Phi { .. }` | union of source types |

**Control flow refinement:**

At a `Match` terminator with `Type(t)` pattern:
- In the matching arm: value has type `{t}`

**Optimizations enabled:**
- Remove impossible Match arms (type not in TypeSet)
- Specialize polymorphic operations when type is known

### Pass 5: Cleanup Constant Folding

**Goal:** Fold constants exposed by earlier passes.

After Guard elimination and CFG simplification, new constant folding opportunities
may emerge. This pass runs the same constant folding logic as Pass 1 to clean up.

**Transformations:**

- Fold `Call` to const builtins: `core::add(1, 2)` → `Const(3)`
- Simplify `If` terminators: `If { condition: Const(true), .. }` → `Jump { target: then }`
- Replace variable references with `Const` instructions when value is known

### Pass 6: Dead Code Elimination (Planned)

**Goal:** Remove computations whose results are never used.

**Algorithm:**

1. Mark as "live":
   - Variables used in `Return`, `Exit` terminators
   - Variables used in `SetIndex` (side effect)
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
│   ├── definedness.rs     # Pass 2: Definedness analysis + diagnostics
│   ├── guard_elim.rs      # Pass 3: Guard elimination + CFG simplification
│   └── type_refinement.rs # Pass 4: Type refinement
```

### Fixed-Point Iteration

Some passes may enable further optimizations by others. The pipeline can iterate:

```rust
loop {
    let changed = false;
    changed |= guard_eliminate(&mut ir);
    changed |= simplify_cfg(&mut ir);
    changed |= const_propagate(&mut ir);
    changed |= dce(&mut ir);
    if !changed { break; }
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

**Why allow explicit `with`?** Self-documenting code. When you write `fn process(with bundle)`,
it signals intent: "I will mutate this parameter." The explicit keyword is optional but encouraged
for clarity.

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
if with UInt(n) = bundle.priority {
    n += 1;  // Mutates bundle.priority
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
DTN bundle blocks without materializing them all into an Array).

---

## Attributes

Functions can be annotated with attributes that provide metadata for the compiler
and driver. Attributes use Rust-style syntax.

### Syntax

```
#[name]                           // Flag attribute
#[name(arg1, arg2)]               // With arguments
#[name(key: value)]               // Named argument
#[name(arg, key: value, flag)]    // Mixed
```

### Argument Forms

| Form | Example | Use |
|------|---------|-----|
| Flag | `export` | Boolean markers |
| Identifier | `validate` | References to other functions |
| Literal | `5000`, `"text"` | Configuration values |
| Named | `timeout: 5000` | Key-value configuration |

### Standard Attributes

| Attribute | Purpose |
|-----------|---------|
| `#[export]` | Mark as externally callable entry point |
| `#[after(fn1, fn2)]` | Ordering dependencies |
| `#[pure]` | No side effects (optimization hint) |

### AST Representation

```rust
pub struct Attribute {
    pub name: Identifier,
    pub args: Vec<AttributeArg>,
}

pub enum AttributeArg {
    Flag(Identifier),                        // export
    Literal(Literal),                        // 5000
    Named { key: Identifier, value: Literal }, // timeout: 5000
}
```

### Attribute Registry

Drivers can register custom attributes, similar to builtins:

```rust
let mut attrs = AttributeRegistry::new();

attrs.register("after", |args| {
    // args contains identifiers for dependencies
    Ok(AttributeValue::RunAfter(args.to_vec()))
});

attrs.register("priority", |args| {
    // Custom driver attribute
    Ok(AttributeValue::Custom("priority", args))
});
```

### Example

```
#[export]
#[after(validate_signature)]
#[config(timeout: 5000, retries: 3)]
fn process_bundle(bundle) {
    if bundle.age > MAX_TTL {
        exit(LIFETIME_EXPIRED);
    }
}
```

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
import std.bpsec;                    // → namespace `bpsec`
import std.cbor.utils;               // → namespace `utils`
import std.status_report.codes as c; // → namespace `c` (explicit alias)

// Local files - string path, filename becomes namespace
import "../common/validation.rill";  // → namespace `validation`
import "./helpers.rill" as h;        // → namespace `h` (explicit alias)
```

### Namespace Qualification

All namespace-qualified access uses `::` separator at call sites:

```rill
// Function calls
bpsec::validate(bundle)         // Function from imported module
utils::decode(bytes)            // Function from imported module
console::log("hello")           // Embedding-provided namespace

// Constant access
codes::LifetimeExpired          // Constant from imported module
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

fn process(bundle) { ... }       // public — embedder can call this
fn validate(bundle) { ... }      // public — embedder can call this
const MAX_TTL = 86400;           // public — visible to embedder

// helpers.rill — all functions here are private
fn compute_checksum(data) { ... } // private — only callable from root
fn internal_helper() { ... }      // private — if unused, eliminated by DCE
```

This means there is no such thing as an "unused function" in the root file —
every root function is a potential entry point. Imported functions that are
never referenced are dead code and can be eliminated during compilation.

### Reserved Namespaces

| Namespace | Purpose | User-definable? |
|-----------|---------|-----------------|
| `core` | Primitives + intrinsics (`core::eq`, `core::is_some`) | No - reserved |
| (embedding) | Host-provided (`console`, `file`, etc.) | Registered by host |
| (user) | Imported modules, local definitions | Yes |

### The `core::` Namespace

The `core::` namespace contains both **builtins** (runtime functions) and
**intrinsics** (compile-time expansions):

```rill
// Builtins - runtime calls
core::eq(a, b)           // Equality comparison
core::add(a, b)          // Addition
core::len(arr)           // Collection length

// Intrinsics - compile-time expansion
core::is_some(x)         // → Guard + Phi
core::is_uint(x)         // → Match + Phi
```

Users cannot define a `core` module or import anything as `core`.

### Name Lookup Order

When resolving an unqualified function call, the lowerer searches in order:

1. **Local functions** - Defined in current module
2. **Imported functions** - From `import` statements
3. **Prelude** - Unqualified names from `core::` (embedding's choice)

This means users can shadow prelude functions:

```rill
fn is_some(x) { x != 0 }  // User's version

is_some(value)            // Calls user's function (shadows prelude)
core::is_some(value)      // Always calls intrinsic (Guard+Phi expansion)
```

Qualified calls (`core::name`, `module::name`) bypass local lookup and go
directly to the specified namespace.

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

### Prelude

All `core::` functions are automatically available as unqualified names. The
prelude is implicit - no configuration needed.

**Builtins** (registered in `standard_builtins()`):
```rill
len(arr)        // → core::len (runtime call)
exit()          // → core::exit (runtime call, exits)
```

**Intrinsics** (compiler-recognized, expand to IR):
```rill
is_some(x)      // → core::is_some (Guard + Phi expansion)
is_uint(x)      // → core::is_uint (Match + Phi expansion)
is_int(x)       // → core::is_int
is_float(x)     // → core::is_float
is_bool(x)      // → core::is_bool
is_text(x)      // → core::is_text
is_bytes(x)     // → core::is_bytes
is_array(x)     // → core::is_array
is_map(x)       // → core::is_map
```

The prelude is essentially `use core::*;` applied implicitly. Users can shadow
any prelude name by defining their own function, but can always access the
original via explicit `core::` qualification.

---

## Builtins and Intrinsics

Rill distinguishes between two types of "built-in" functionality:

| Concept | Definition | When Evaluated | Can Handle Undefined? |
|---------|------------|----------------|----------------------|
| **Builtin** | Rust function linked into the interpreter | Runtime (VM execution) | No - short-circuited before call |
| **Intrinsic** | Syntactic construct that expands to IR | Compile time (lowering) | Yes - expands to Guard-based control flow |

### Design Philosophy

The core principle is **minimal semantics**: Rill has very few intrinsics, keeping the
language simple and consistent. Users can implement convenience functions themselves
with no performance penalty (they compile to the same IR).

---

## Intrinsics

Intrinsics are syntactic constructs that expand to IR sequences during AST→IR lowering.
They are not function calls - they generate inline control flow.

### Current Intrinsics

| Syntax | Expands To | Purpose |
|--------|------------|---------|
| `x + y` | `Call core::add(x, y)` | Arithmetic (maps to builtin) |
| `x && y` | `Guard x → (evaluate y), (false)` + Phi | Short-circuit AND |
| `x \|\| y` | `Guard x → (true), (evaluate y)` + Phi | Short-circuit OR |
| `x != y` | `Call core::not(Call core::eq(x, y))` | Reflexive comparison |
| `x > y` | `Call core::lt(y, x)` | Reflexive comparison (swap) |
| `x <= y` | `Call core::not(Call core::lt(y, x))` | Reflexive comparison |
| `x >= y` | `Call core::not(Call core::lt(x, y))` | Reflexive comparison |
| `[a, b, c]` | `Call core::make_array(a, b, c)` | Array literal |
| `{k: v, ...}` | `Call core::make_map(k, v, ...)` | Map literal |
| `if cond { a } else { b }` | `If` terminator + blocks + Phi | Conditional |
| `if let p = x { a } else { b }` | `Guard` + pattern match + blocks + Phi | Undefined-aware conditional |
| `arr[i] = v` (lvalue) | `Index` + `Guard` + `SetIndex` + Phi | Short-circuit assignment |
| `is_some(x)` | `Guard x → (true), (false)` + Phi | Check if defined |
| `is_uint(x)` | `Match x → UInt:(true), default:(false)` + Phi | Type check |
| `is_int(x)` | `Match x → Int:(true), default:(false)` + Phi | Type check |
| `is_float(x)` | `Match x → Float:(true), default:(false)` + Phi | Type check |
| `is_bool(x)` | `Match x → Bool:(true), default:(false)` + Phi | Type check |
| `is_text(x)` | `Match x → Text:(true), default:(false)` + Phi | Type check |
| `is_bytes(x)` | `Match x → Bytes:(true), default:(false)` + Phi | Type check |
| `is_array(x)` | `Match x → Array:(true), default:(false)` + Phi | Type check |
| `is_map(x)` | `Match x → Map:(true), default:(false)` + Phi | Type check |

### Reflexive Comparison Operators

The comparison operators `!=`, `>`, `<=`, `>=` are implemented as intrinsics that
expand to combinations of the primitive builtins `core::eq`, `core::lt`, and `core::not`:

- `a != b` → `not(eq(a, b))`
- `a > b` → `lt(b, a)` (operands swapped)
- `a <= b` → `not(lt(b, a))`
- `a >= b` → `not(lt(a, b))`

This reduces the builtin set to just `eq` and `lt` for comparisons, which is
sufficient because Rill uses `undefined` instead of IEEE-754 NaN. Without NaN's
special comparison semantics (where `NaN != NaN` is true), mathematical reflexivity
holds and these expansions are equivalent to dedicated operators.

### Function-like Intrinsics

The `is_*` family are function-like intrinsics that expand to control flow.

**`is_some(x)`** - Checks whether a value is defined:

```rill
if is_some(arr[100]) {
    // arr[100] exists
}
```

Expands to:
```
Block N:
  Guard x → defined: N+1, undefined: N+2

Block N+1:
  result_true = const true
  Jump N+3

Block N+2:
  result_false = const false
  Jump N+3

Block N+3:
  result = Phi [(N+1, result_true), (N+2, result_false)]
```

This is exactly the same IR as `if let _ = x { true } else { false }`.

**`is_uint(x)`, `is_int(x)`, etc.** - Type checking intrinsics:

```rill
if is_uint(value) {
    // value is a UInt
}
```

Expands to a type match:
```
Block N:
  Match x → UInt: N+1, default: N+2

Block N+1:
  result_true = const true
  Jump N+3

Block N+2:
  result_false = const false
  Jump N+3

Block N+3:
  result = Phi [(N+1, result_true), (N+2, result_false)]
```

This is equivalent to `if let UInt(_) = x { true } else { false }`.

### Why Not More Intrinsics?

Functions like `default(x, fallback)` or `coalesce(a, b, c)` were considered but
rejected as unnecessary. Users can define them with no penalty:

```rill
fn default(value, fallback) {
    if let v = value { v } else { fallback }
}
```

This compiles to identical IR as a hypothetical intrinsic would.

---

## Builtin System

Builtins are Rust functions registered with metadata that drives compiler decisions.
They execute at runtime in the VM. Follows Lua embedding API patterns.

### Key Property: Builtins Never Receive Undefined

The IR short-circuits via Guards before calling any builtin. If an argument might
be undefined, the call is wrapped in control flow that skips it when undefined.
This means builtin implementations don't need to handle undefined inputs.

### Builtin Metadata

```rust
struct BuiltinMeta {
    params: Vec<ParamSpec>,      // Parameter types and optionality
    returns: ReturnBehavior,     // Returns or Exits
    purity: Purity,              // Optimization potential + fallibility
}

enum ReturnBehavior {
    /// Returns a value of this type to the caller
    Returns(TypeSig),

    /// Never returns - exits to driver with typed value
    /// Lowers to Terminator::Exit
    Exits(TypeSig),
}

/// Function pointer for compile-time evaluation
type ConstEvalFn = fn(&[ConstValue]) -> Option<ConstValue>;

enum Purity {
    /// Has side effects or depends on external state
    /// Implicitly fallible - may return undefined due to external factors
    Impure,

    /// No side effects, deterministic given same inputs
    /// fallible: true if domain errors possible (overflow, type mismatch)
    Pure { fallible: bool },

    /// Can be evaluated at compile time
    /// fallible: true if domain errors possible
    Const { eval: ConstEvalFn, fallible: bool },
}
```

### Purity and Fallibility

Fallibility indicates whether an operation may return undefined:

| Purity | Fallible | May Return Undefined? | Example |
|--------|----------|----------------------|---------|
| `Impure` | (always) | Yes - external factors | I/O operations |
| `Pure { fallible: true }` | Yes | Yes - domain errors | - |
| `Pure { fallible: false }` | No | No - always succeeds | - |
| `Const { fallible: true, .. }` | Yes | Yes - overflow, type errors | `core::add` |
| `Const { fallible: false, .. }` | No | No - always succeeds | `core::make_array` |

The optimizer uses `purity.may_return_undefined()` to determine definedness of results.

### Builtin Registry

```rust
struct BuiltinRegistry {
    builtins: HashMap<String, BuiltinDef>,
}

struct BuiltinDef {
    name: String,
    implementation: BuiltinImpl,
    meta: BuiltinMeta,
}

enum BuiltinImpl {
    Native(fn(&mut VM, &[Value]) -> ExecResult),
    Closure(Box<dyn Fn(&mut VM, &[Value]) -> ExecResult>),
}

enum ExecResult {
    Return(Option<Value>),  // Normal return (None = undefined)
    Exit(Value),            // Hard exit to driver
}
```

### Lowering Behavior

During IR lowering, builtin calls are resolved:

1. If `returns` is `Exits(_)` → emit `Terminator::Exit`
2. If `purity` is `Const { eval, .. }`:
   - Valid in const expressions
   - If all arguments are const, call `eval` to compute result at compile time
3. If `purity` is `Pure` or `Const` → can be reordered/eliminated/CSE'd

### Current Builtins

**Core primitives** (used in IR, map to VM opcodes):

| Category | Builtins | Fallible | Notes |
|----------|----------|----------|-------|
| Arithmetic | `core::add`, `sub`, `mul`, `div`, `mod`, `neg` | Yes | Overflow possible |
| Comparison | `core::eq`, `lt` | Yes | Type mismatch possible |
| Bitwise | `core::bit_and`, `bit_or`, `bit_xor`, `bit_not`, `shl`, `shr` | Yes | Type mismatch |
| Bitwise | `core::bit_test`, `bit_set` | Yes | OOB if bit >= 64 |
| Logical | `core::not` | Yes | Type mismatch |
| Collections | `core::make_array` | **No** | Always succeeds |
| Collections | `core::make_map` | Yes | Fails on odd arg count |
| Utility | `len` | Yes | Type mismatch |

**External functions** (runtime dispatch to Rust):

| Function | Fallible | Notes |
|----------|----------|-------|
| `drop` | N/A | Diverges (Exits to driver) |

Embedding environments add more external functions (e.g., `console.log`, `file.read`).
These are just function calls at every level - no special IR or codegen treatment.

### Three-Phase Compilation Model

Operators and builtins are handled differently at each compilation phase:

| Phase | Handling | Example: `a >= b` |
|-------|----------|-------------------|
| **Const eval** | Chain const evaluators directly → `ConstValue` | `const_eval_lt(a,b)` then `const_eval_not(result)` |
| **IR lowering** | Expand to primitive `Call` instructions | `Call core::lt(a,b)` then `Call core::not(result)` |
| **Codegen** | Peephole pattern match → efficient VM opcodes | `not(lt(a,b))` → `GEQ a, b` |

**Why this design:**

1. **Const eval**: Direct evaluation, no intermediate representation needed
2. **IR lowering**: Expand reflexive operators to primitives for maximum optimization
   - Enables: CSE, dead code elimination, double-negation removal, branch inversion
3. **Codegen**: Pattern match to recover efficient instructions
   - `not(eq(a,b))` → `NEQ`
   - `not(lt(b,a))` → `LEQ`
   - `lt(b,a)` → `GT`

**Builtin categories:**

| Category | IR Level | VM Level | Example |
|----------|----------|----------|---------|
| **Primitives** | `Call` instruction | Direct opcode | `core::eq`, `core::add` |
| **Codegen-only** | Expanded to primitives | Dedicated opcode | `neq`, `leq`, `gt`, `geq` |
| **External** | `Call` instruction | Runtime dispatch | `drop`, `console.log` |

Codegen-only instructions exist to optimize common patterns but aren't needed at IR
level where the expanded form enables better optimization.

### Example Registration

```rust
// Array construction - infallible (always succeeds)
BuiltinDef::new("core::make_array", builtin_make_array)
    .returns(TypeSig::of(BaseType::Array))
    .const_eval_infallible(const_eval_make_array)

// Addition - fallible (overflow possible)
BuiltinDef::new("core::add", builtin_add)
    .param("a", TypeSig::numeric())
    .param("b", TypeSig::numeric())
    .returns(TypeSig::numeric())
    .const_eval(const_eval_add)  // fallible by default

// Exit - diverges, implicitly impure
BuiltinDef::new("drop", builtin_drop)
    .param_optional("reason", TypeSig::uint())
    .exits(TypeSig::uint())
```

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
    attributes: Vec<Attribute>,
}

struct ParamMeta {
    name: String,
    type_sig: TypeSignature,
    by_ref: bool,
}

enum Attribute {
    RunAfter(Vec<String>),  // Execution ordering dependencies
    EntryPoint,             // Externally callable
    Pure,                   // No side effects
}
```

### Host Driver Binding

The host driver loads compiled modules and selects functions by signature:

```rust
// Find functions matching desired signature
let handlers: Vec<_> = compiled.functions
    .iter()
    .filter(|f| matches_signature(f, &expected_sig))
    .collect();

// Optionally sort by RunAfter dependencies
let ordered = topo_sort(handlers);

// Execute
for handler in ordered {
    let result = call(handler, &mut context);
    // Handle result based on application needs
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
            attrs: [Attribute...],
            code: Tag(0xF1702) [Instruction...],
        },
        ...
    ],
    constants: [ConstBinding...],
}
```

### Benefits

- **Self-describing**: CBOR is schema-flexible
- **Compact**: Efficient binary encoding
- **Extensible**: Custom tags for future features
- **Portable**: No platform-specific format dependencies

---

## Example Use Case: DTN Bundle Filtering

While Rill is a general-purpose embeddable scripting language, it was originally
designed for DTN bundle processing. This section demonstrates how Rill can be used
in that context as an example of embedding the language in a domain-specific application.

### Filter Functions

A bundle filter is a function that takes a `Bundle` parameter (by reference)
and uses the `exit()` builtin to reject bundles:

```
fn check_lifetime(bundle) {
    if bundle.age > MAX_TTL {
        exit(LIFETIME_EXPIRED);
    }
    // Implicit: bundle continues if exit() not called
}

fn validate_destination(bundle) {
    if !is_valid_eid(bundle.destination) {
        exit(INVALID_DESTINATION);
    }
}
```

### The `exit()` Builtin

The `exit(code)` builtin is a diverging function that exits the script
and returns a disposition to the host driver:

```rust
// Registration
registry.register(
    BuiltinDef::new("drop", builtin_drop)
        .param_optional("reason", TypeSig::uint())
        .exits(TypeSig::uint())
        .purity(Purity::Impure)
);

// Implementation
fn builtin_drop(_vm: &mut VM, args: &[Value]) -> Result<ExecResult, ExecError> {
    let reason = args.first().cloned().unwrap_or(Value::UInt(0));
    Ok(ExecResult::exit(reason))
}
```

### Filter Chain Execution

The host driver runs filters as a chain:

```rust
// Find filter functions (accept Bundle, may call exit())
let filters: Vec<_> = compiled.functions
    .iter()
    .filter(|f| f.params.get(0).map(|p| p.type_sig == TypeSig::Bundle).unwrap_or(false))
    .collect();

// Sort by RunAfter dependencies
let ordered = topo_sort(filters);

// Execute filter chain
for filter in ordered {
    match vm.call(filter, &mut bundle) {
        Ok(_) => continue,  // Filter passed, continue chain
        Err(ExitValue(reason)) => {
            // Filter called exit()
            return FilterResult::Drop(reason);
        }
    }
}
FilterResult::Continue
```

### Ordering Dependencies

Filters can declare ordering constraints via the `RunAfter` attribute:

```
#[after(validate_signature)]
fn check_payload(bundle) {
    // Only runs after validate_signature
}
```

The driver topologically sorts filters to honor these dependencies.

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
- [x] Reference binding via Slot::Ref
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

Duck typing philosophy. Scripts can probe values without try/catch. Failed operations naturally propagate. Matches CBOR's flexible type model.

### Why IndexMap for maps?

Preserves insertion order (important for CBOR), provides O(1) lookup, and can be hashed for use as map keys (manual Hash impl iterates in order).

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

### Why no special `filter` keyword?

The language is general-purpose. Making all functions uniform and using metadata
for driver binding enables:

- Multiple use cases (filtering, transforms, validation, etc.)
- Driver flexibility (select by signature, not syntax)
- Cleaner language (fewer keywords, uniform semantics)
- `exit()` as a builtin rather than special syntax

### Why Rust-style attributes with `:` for named values?

Attributes provide extensible metadata without language keywords:

- Rust-style `#[attr]` is familiar and visually distinct from code
- Using `:` instead of `=` for named values avoids confusion with assignment
- Consistent with map literal syntax (`{key: value}`)
- Drivers can register custom attributes, like builtins
- Attributes compile to metadata, not runtime code

### Why CBOR for compiled binary format?

The language has first-class CBOR support, so using CBOR for the compiled format
is natural:

- Same tooling for inspection
- Natural representation of constants (already CBOR values)
- Extensible via custom tags
- No dependency on platform-specific formats
- Self-describing format consistent with language philosophy

### Why separate Definedness and Type analysis passes?

The TypeSet structure has two orthogonal axes:

```rust
pub struct TypeSet {
    pub types: BTreeSet<BaseType>,   // Concrete types: Bool, UInt, etc.
    pub maybe_undefined: bool,        // Could be undefined?
}
```

These are independent concerns:
- A value can be "definitely defined" without knowing its type
- A value can have a known type but still be "maybe undefined"

Splitting the analyses provides:

1. **Earlier CFG simplification**: Definedness analysis removes Guards, simplifying
   the control flow graph before type analysis runs
2. **Cleaner lattices**: Each pass has a simple, well-defined lattice
3. **Better diagnostics**: Definedness errors ("value is undefined") are distinct
   from type errors ("expected UInt, got Text")
4. **Efficiency**: Type refinement runs on a simpler CFG with fewer blocks

The alternative (unified analysis) would require a product lattice of
`Definedness × TypeSet`, which is more complex and doesn't naturally separate
the two kinds of errors.

---

*Last updated: Updated optimization pipeline to reflect implemented passes.*
