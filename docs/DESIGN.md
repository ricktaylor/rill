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
```

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
| **Function** | Everything else (may be inlined or called) | `core.add`, `len()`, `drop()` |

**Intrinsics** are minimal - only `And` (`&&`) and `Or` (`||`) which require
short-circuit evaluation (control flow to skip the second operand).

**Core builtins** (`core.*`) implement all other operators with `Purity::Const`:

- Arithmetic: `core.add`, `core.sub`, `core.mul`, `core.div`, `core.mod`, `core.neg`
- Comparison: `core.eq`, `core.lt`
- Logical: `core.not`
- Bitwise: `core.bit_and`, `core.bit_or`, `core.bit_xor`, `core.bit_not`, `core.shl`, `core.shr`

These const builtins enable compile-time folding: `1 + 2` lowers to `Call("core.add", [1, 2])`
which the optimizer folds to `3` using the const evaluator.

**Other functions** include user-defined, prelude, and host-provided:

- *Prelude*: `len()`, `concat()`, `to_uint()`, `is_uint()`, `is_some()`, etc.
- *Host*: `drop()`, `decode()`, `validate()`

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
| Comparison | `==` `<` | `core.eq`, `core.lt` |
| Arithmetic | `+` `-` `*` `/` `%` `-x` | `core.add`, `core.sub`, `core.mul`, `core.div`, `core.mod`, `core.neg` |
| Logical | `!` | `core.not` |
| Bitwise | `&` `\|` `^` `~` `<<` `>>` | `core.bit_and`, `core.bit_or`, `core.bit_xor`, `core.bit_not`, `core.shl`, `core.shr` |

Other builtins with appropriate purity annotations:

- Collection: `len()`, `concat()`, `push()`, `insert()`, `slice()`
- Ranges: `range()`, `range_inclusive()`
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

## Language Features

### Binding Modes

The language uses a consistent pattern: **default is by-reference**, use `let` to opt into by-value.
The `with` keyword can be used explicitly for by-reference (same as default) for symmetry and clarity.

| Context | by-ref (explicit) | by-ref (implicit) | by-value |
|---------|-------------------|-------------------|----------|
| Statement | `with x = expr` | — | `let x = expr` |
| Conditional | `if with x = expr { }` | — | `if let x = expr { }` |
| For loop | `for with x in arr { }` | `for x in arr { }` | `for let x in arr { }` |
| Match arm | `with pat => { }` | `pat => { }` | `let pat => { }` |
| Function param | `fn foo(with x)` | `fn foo(x)` | `fn foo(let x)` |

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
| `len(v)` | `UInt` or Undefined | Collection length |
| `concat(a, b)` | Collection or Undefined | Concatenate arrays, text, bytes |
| `slice(coll, start, end)` | Collection | Extract sub-collection [start, end) |
| `range(start, end)` | Array | Create range [start, end) |
| `range_inclusive(start, end)` | Array | Create range [start, end] |

**Internal functions** (compiler-generated for lowering literals):

| Function | Returns | Used For |
|----------|---------|----------|
| `make_array()` | Array | `[a, b, c]` literals |
| `make_map()` | Map | `{k: v}` literals |
| `push(coll, elem)` | Collection | Building collection literals |
| `insert(map, k, v)` | Map | Building map literals |

### Rest Patterns

```rust
let [first, ..rest] = arr;      // rest = remaining elements
let [head, .., tail] = arr;     // ignore middle
let [a, ..middle, z] = arr;     // capture middle
let [first, ..] = arr;          // ignore rest (no binding)
```

### Ranges

```rust
0..10     // Exclusive: [0, 1, ..., 9]
0..=10    // Inclusive: [0, 1, ..., 10]

for i in 0..len(arr) { }        // Dynamic bounds
let indices = 0..5;             // Range as value
```

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
        drop(LIFETIME_EXPIRED);
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

```rust
// Standard library (dotted path)
import std.bpsec;
import std.status_report.codes as codes;

// Local files (quoted string)
import "../common/validation.flt";
import "./helpers.flt" as helpers;
```

### Namespace Resolution

```rust
codes::LifetimeExpired          // Constant from imported module
bpsec::validate_signature(...)  // Function from imported module
len(arr)                        // Prelude (no namespace)
```

### Prelude (Auto-imported)

**Always inlined:**

- `is_uint(x)`, `is_int(x)`, ... - Type checks (→ Match + Phi)
- `is_some(x)` - Existence check (→ Guard + Phi)

**Regular functions:**

- `to_uint(x)`, `to_int(x)`, ... - Type conversions
- `len(x)` - Collection length
- `concat(a, b)` - Concatenate collections
- `slice(coll, start, end)` - Extract sub-collection
- `range(start, end)`, `range_inclusive(start, end)` - Create ranges

---

## Builtin System

Builtins are registered with metadata that drives compiler lowering decisions.
Follows Lua embedding API patterns.

### Builtin Metadata

```rust
struct BuiltinMeta {
    params: Vec<ParamSpec>,
    returns: ReturnBehavior,
    purity: Purity,
}

enum ReturnBehavior {
    /// Returns a value of this type (may include maybe_undefined)
    Returns(TypeSignature),

    /// Never returns to caller - exits to driver with typed value
    Exits(TypeSignature),

    // Future: Yields(TypeSignature) for generators/async
}

/// Function pointer for compile-time evaluation
type ConstEvalFn = fn(&[ConstValue]) -> Option<ConstValue>;

enum Purity {
    /// Has side effects, runtime-dependent
    Impure,

    /// No side effects, deterministic - enables optimization
    Pure,

    /// Can evaluate at compile time with the provided evaluator
    /// Valid in const initializers. Implies Pure.
    Const(ConstEvalFn),
}
```

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
2. If `purity` is `Const(eval_fn)`:
   - Valid in const expressions
   - If all arguments are const, call `eval_fn` to compute result at compile time
3. If `purity` is `Pure` or `Const(_)` → can be reordered/eliminated/CSE'd

### Example Registrations

```rust
// Const evaluator for len - computes length at compile time
fn const_eval_len(args: &[ConstValue]) -> Option<ConstValue> {
    let value = args.first()?;
    let len = match value {
        ConstValue::Text(s) => s.chars().count() as u64,
        ConstValue::Bytes(b) => b.len() as u64,
        ConstValue::Array(arr) => arr.len() as u64,
        ConstValue::Map(map) => map.len() as u64,
        _ => return None,
    };
    Some(ConstValue::UInt(len))
}

// len: const with evaluator, returns UInt
BuiltinDef {
    name: "len",
    meta: BuiltinMeta {
        params: vec![ParamSpec::required("v", TypeSig::Collection)],
        returns: ReturnBehavior::Returns(TypeSig::uint()),
        purity: Purity::Const(const_eval_len),
    },
    ..
}

// drop: impure, exits with UInt (no const evaluator)
BuiltinDef {
    name: "drop",
    meta: BuiltinMeta {
        params: vec![ParamSpec::optional("reason", TypeSig::UInt)],
        returns: ReturnBehavior::Exits(TypeSig::uint()),
        purity: Purity::Impure,
    },
    ..
}

// decode: pure but not const (runtime-only operation)
BuiltinDef {
    name: "decode",
    meta: BuiltinMeta {
        params: vec![ParamSpec::required("bytes", TypeSig::Bytes)],
        returns: ReturnBehavior::Returns(TypeSig::any()),
        purity: Purity::Pure,
    },
    ..
}
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
and uses the `drop()` builtin to reject bundles:

```
fn check_lifetime(bundle) {
    if bundle.age > MAX_TTL {
        drop(LIFETIME_EXPIRED);
    }
    // Implicit: bundle continues if drop() not called
}

fn validate_destination(bundle) {
    if !is_valid_eid(bundle.destination) {
        drop(INVALID_DESTINATION);
    }
}
```

### The `drop()` Builtin

The `drop(reason)` builtin is a diverging function that exits the script
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
// Find filter functions (accept Bundle, may call drop())
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
            // Filter called drop()
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
- [x] VM core (stack, frames, slots)
- [x] Heap tracking with HeapVal
- [x] Value types with Hash/Eq
- [x] Call convention with return slots
- [x] Reference binding via Slot::Ref

### In Progress

- [ ] IR lowering (AST → IR) - control flow primitives designed
- [ ] Builtin registry and metadata system

### Pending

- [ ] Instruction execution
- [ ] Builtin implementations
- [ ] Standard library modules
- [ ] CBOR encode/decode integration
- [ ] CBOR binary output format
- [ ] Optimizer passes

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

Drop::drop() takes no arguments, so deallocation tracking requires storing the heap reference somewhere accessible. By embedding HeapRef in the Rc'd allocation (Tracked<T>), HeapVal remains 8 bytes (one pointer). The cost is 8 extra bytes per allocation, not per HeapVal clone. This keeps Value at 16 bytes for better cache locality across the 65K-slot stack—a bigger win than saving 8 bytes per allocation.

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
- `drop()` as a builtin rather than special syntax

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

---

*Last updated: Added variadic function support with rest parameters (`..args`).*
