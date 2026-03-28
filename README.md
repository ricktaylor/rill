# Rill

> A memory-safe, embeddable scripting language.

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

**Rill** is a general-purpose scripting language designed for high-reliability environments where script failures cannot compromise the host process. Think of it as a duck-typed subset of Rust with the embeddability of Lua and the safety guarantees of its Rust heritage.

## Origin Story

Rill emerged from work on [Hardy](https://github.com/ricktaylor/hardy), a Delay-Tolerant Networking (RFC 9171) agent for satellite communications. When you're managing traffic policies during 40-minute light-speed delays, you need a scripting engine that won't crash your agent or leak memory while waiting for Mars to respond.

Unable to find a scripting language that felt truly "at home" in a high-reliability Rust environment, Rill was created to fill that gap.

## Why "Rill"?

A *rill* is a small stream — a modest channel through which water flows. In lunar geology, *rilles* are the sinuous channels carved across the Moon's surface, remnants of ancient lava flows. Like these channels, Rill is designed to be a conduit: small, efficient pathways for data to flow through your systems. A fitting name for a language born in deep space communications.

## Key Features

- **Memory Safe**: Guaranteed resource limits (stack, heap) with no undefined behavior
- **Rust-like Syntax**: Familiar to Rust developers, but duck-typed and without fighting the borrow checker
- **Duck-Typed Values**: Practical scalar and collection types (Bool, UInt, Int, Float, Text, Bytes, Array, Map) — the common denominator of JSON, CBOR, MessagePack, and similar data interchange formats
- **Embeddable**: Builtin registration system for seamless host integration
- **Optimizing Compiler**: SSA IR with type inference, specialization, and 11 optimization passes — compiles to closure-threaded code, not a bytecode interpreter
- **Pattern Matching**: Rich destructuring with type narrowing and rest patterns
- **Reference Semantics**: Explicit `with` bindings for in-place mutation vs `let` for value copies
- **No Exceptions**: No error types, no panics, no stack unwinding — failed operations produce `undefined` values that propagate silently until handled. Scripts never crash; the host always gets a clean result.
- **Lightweight**: Minimal dependencies, designed for resource-constrained environments

## Quick Example

```rust
// Data validation and transformation
fn check_lifetime(bundle) {
    if bundle.age > MAX_TTL {
        exit(codes::LifetimeExpired);
    }
}

fn validate_payload(bundle) {
    match bundle.payload {
        Bytes(data) if len(data) > 0 => {
            // Process valid payload
        },
        _ => exit(codes::BlockUnintelligible)
    }
}

// Pattern matching with type narrowing and in-place mutation
fn process_priority(bundle) {
    if with UInt(priority) = bundle.priority {
        priority += 1;  // Mutates bundle.priority in place
    }
}
```

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
│   IR    │  SSA form with type inference
└────┬────┘
     │
     ▼
┌──────────────┐
│  Optimizer   │  11 passes: const fold, type refinement, coercion,
│              │  guard elim, definedness, copy prop, DCE, CSE,
│              │  algebra, cast elision, ref elision
└──────┬───────┘
       │
       ▼
┌──────────────┐
│   Compiler   │  Closure-threaded with type specialization
└──────┬───────┘
       │
       ▼
┌──────────────┐
│   Executor   │  Flat PC-based loop, stack + heap tracking
└──────────────┘
```

- **Parser**: Built with [chumsky](https://github.com/zesterer/chumsky) combinator library
- **IR**: Single Static Assignment with loop-carried phis, `with` reference tracking
- **Optimizer**: Two-phase pipeline — coarse (pre-type-info) and type-informed (post-refinement), with interprocedural analysis and function monomorphization
- **Compiler**: Type-specialized closures — emits direct `u64::checked_add` instead of runtime type dispatch when types are provably known
- **Executor**: Stack-based with accurate capacity-based heap tracking and configurable resource limits

## Language Highlights

### Duck-Typed Values

Rill's type system covers the common ground across structured data formats — the same types you'd find in JSON, CBOR, or MessagePack:

```rust
let data = {
    timestamp: 1234567890,
    payload: bytes([0x48, 0x65, 0x6c, 0x6c, 0x6f]),
    metadata: {
        priority: 10,
        destination: "earth"
    }
};
```

No type annotations needed — the optimizer infers types and specializes arithmetic at compile time.

### Reference vs Value Semantics

```rust
// let creates a copy
let x = bundle.priority;
x += 1;  // Does NOT modify bundle.priority

// with creates a reference
if with UInt(priority) = bundle.priority {
    priority += 1;  // DOES modify bundle.priority
}
```

### Pattern Matching

```rust
match bundle.payload {
    Array([first, ..rest]) => {
        // first: first element, rest: remaining elements
    },
    Map(m) if len(m) > 0 => {
        // m is a non-empty map
    },
    UInt(n) => {
        // n is the unwrapped uint value
    },
    _ => {
        // Default case
    }
}
```

### No Exceptions — Just Undefined

Rill has no error type, no exceptions, and no stack unwinding. Operations that can't produce a meaningful result — a missing map key, division by zero, type mismatches — simply return `undefined`. Undefined propagates through subsequent operations without interrupting the script:

```rust
let x = data.missing_field.nested;  // → undefined (no KeyError)
let y = x + 10;                     // → undefined (not a runtime error)
let z = 1 / 0;                      // → undefined (not a panic)

// Use if let to branch on presence
if let value = potentially_undefined {
    // value is guaranteed to be defined here
}
```

This means scripts never crash mid-execution. The host always gets a clean result — either a defined value or `undefined` — making Rill predictable in environments where partial failures must not bring down the system.

## Embedding Rill

```rust
use rill::{VM, BuiltinRegistry, Value};

// Register custom builtins
let mut registry = BuiltinRegistry::new();
registry.register(
    BuiltinDef::new("send_report", my_send_impl)
        .param("data", TypeSet::bytes())
        .returns(TypeSet::bool())
        .impure()
);

// Compile and execute
let (program, warnings) = rill::compile(source, &registry)?;
let mut vm = VM::new();

// Push arguments and call
vm.push(Value::UInt(42))?;
let result = program.call(&mut vm, "process", 1)?;

// For hot-path execution, resolve the function once
let process = program.function("process").expect("function exists");
for input in inputs {
    vm.push(input)?;
    let result = process.call(&mut vm, 1)?;
}
```

## Current Status

**Complete:**

- Full parser with implicit return support
- SSA intermediate representation with loop-carried phis
- `with` reference bindings with write-back (MakeRef/WriteRef)
- Pattern matching: type narrowing, destructuring, rest patterns, guards
- Sequence type with lazy ranges and zero-copy array slices
- 11 optimizer passes: constant folding, type refinement, coercion insertion, guard elimination, definedness analysis, copy propagation, dead code elimination, common subexpression elimination, algebraic simplification, cast elision, ref elision
- Interprocedural return type inference and argument type propagation
- Function monomorphization (up to 4 type-specialized variants)
- Closure-threaded compiler with type-specialized arithmetic
- Flat PC-based executor with stack/heap tracking
- Builtin registry with monomorphic variants and purity tracking
- Diagnostics with source spans, line:column formatting, and error codes
- Public API: `compile()`, `Program::call()`, `FunctionHandle` for hot-path execution
- 139+ end-to-end tests passing
- Grammar specification (ABNF) and design documentation

**In Progress:**

- Module/import resolution system
- Standard library (`std.time`, `std.cbor`, `std.encoding`, `std.parsing`)
- Prelude (utility functions: `is_some`, `is_uint`, `default`, etc.)

**Planned:**

- CLI tool (`rill run`, `rill check`, `rill dump`)
- Tail-call optimization, function inlining, loop-invariant code motion
- Bytecode serialization format
- LSP support
- StepKind peephole optimization layer

## Documentation

- **[DESIGN.md](docs/DESIGN.md)**: Comprehensive design document (architecture, rationale, implementation details)
- **[STDLIB.md](docs/STDLIB.md)**: Standard library documentation
- **[grammar.abnf](docs/grammar.abnf)**: Formal ABNF grammar specification
- **[example.txt](docs/example.txt)**: Extensive syntax examples

## Use Cases

While born in the Deep Space Network, Rill is a general-purpose language suitable for:

- **Embedded scripting**: Safe sandboxing for untrusted scripts in Rust applications
- **Data validation and transformation**: Rich pattern matching for structured data
- **Configuration processing**: Complex rule evaluation without compromising host safety
- **Policy engines**: Filter, route, and prioritize based on runtime data
- **Domain-specific applications**: DTN bundle filtering, IoT device policies, network packet inspection

## Building

```bash
cargo build --release
cargo test
```

## Safety Guarantees

Rill scripts run in a controlled sandbox with hard limits:

- **Stack limit**: 65,536 slots (catches deep recursion)
- **Heap limit**: 16MB default (configurable per-VM)
- **No undefined behavior**: All operations are memory-safe
- **Bounded execution**: Resource exhaustion returns errors rather than crashing

Scripts cannot:

- Escape the sandbox
- Crash the host process
- Leak memory beyond the heap limit
- Access host resources except through registered builtins

## Contributing

Feedback is greatly appreciated! Areas of particular interest:

- **Module system**: Import resolution and multi-file programs
- **Standard library**: Core modules (time, encoding, data format codecs)
- **Advanced optimizations**: TCO, inlining, LICM
- **Documentation**: Embedding guide, tutorials, API documentation
- **Testing**: Edge cases, fuzzing, integration tests

Please open issues for bugs, feature requests, or design discussions.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Acknowledgments

- Inspired by the needs of [Hardy](https://github.com/ricktaylor/hardy) and the Deep Space Network
- Built with [chumsky](https://github.com/zesterer/chumsky) parser combinators
- Designed for environments where reliability isn't optional

---

*"Small streams carve deep channels — in rock, in regolith, in code."*
