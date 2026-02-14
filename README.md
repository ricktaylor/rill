# Rill

> A memory-safe, embeddable scripting language.

[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)

**Rill** is a general-purpose scripting language designed for high-reliability environments where script failures cannot compromise the host process. Think of it as a duck-typed subset of Rust with the embeddability of Lua and the safety guarantees of its Rust heritage.

## Origin Story

Rill emerged from work on [Hardy](https://github.com/rick_taylor/hardy), a Delay-Tolerant Networking (RFC 9171) agent for satellite communications. When you're managing traffic policies during 40-minute light-speed delays, you need a scripting engine that won't crash your agent or leak memory while waiting for Mars to respond.

Unable to find a scripting language that felt truly "at home" in a high-reliability Rust environment, Rill was created to fill that gap.

## Why "Rill"?

Rills are the oldest minerals on Earth—chemically inert, heat-resistant, and fundamentally hardy. They survive geological time scales and extreme conditions. A fitting name for a language designed to survive the vacuum of space.

## Key Features

- **Memory Safe**: Guaranteed resource limits (stack, heap) with no undefined behavior
- **Rust-like Syntax**: Familiar to Rust developers, but duck-typed and without fighting the borrow checker
- **CBOR-Native**: First-class support for CBOR data types and manipulation
- **Embeddable**: Lua-style builtin registration system for seamless host integration
- **Pattern Matching**: Rich destructuring with type narrowing and rest patterns
- **Reference Semantics**: Explicit `with` bindings for in-place mutation vs `let` for value copies
- **Lightweight**: Interpreted language optimized for resource-constrained environments

## Quick Example

```rust
// DTN bundle filtering example
import std.status_report.codes as codes;

fn check_lifetime(bundle) {
    if bundle.age > MAX_TTL {
        drop(codes::LifetimeExpired);
    }
}

fn validate_payload(bundle) {
    match bundle.payload {
        Bytes(data) if len(data) > 0 => {
            // Process valid payload
        },
        _ => drop(codes::BlockUnintelligible)
    }
}

// Pattern matching with type narrowing
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
┌─────────┐
│   VM    │  Stack-based with heap tracking
└─────────┘
```

- **Parser**: Built with [chumsky](https://github.com/zesterer/chumsky) combinator library
- **IR**: Single Static Assignment intermediate representation for optimization
- **VM**: Stack-based execution with accurate heap tracking and resource limits

## Language Highlights

### CBOR-First Design

Rill has native support for the CBOR data model:

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

### Undefined Propagation

No need for `?` operators—undefined values propagate naturally:

```rust
let x = undefined_value.field.nested;  // → undefined
let y = x + 10;  // → undefined

// Use if let when you need to handle presence explicitly
if let value = potentially_undefined {
    // value is guaranteed to be defined here
}
```

## Embedding Rill

```rust
use rill::{VM, BuiltinRegistry, Value};

// Register custom builtins
let mut registry = BuiltinRegistry::new();
registry.register(
    BuiltinDef::new("my_function", my_builtin_impl)
        .param("input", TypeSig::text())
        .returns(TypeSig::uint())
        .purity(Purity::Pure)
);

// Compile and execute
let program = rill::parse(source)?;
let compiled = rill::compile(program, &registry)?;
let mut vm = VM::new(compiled);

match vm.call_function("main", &[]) {
    Ok(result) => println!("Result: {:?}", result),
    Err(e) => eprintln!("Script error: {}", e),
}
```

## Current Status

⚠️ **Early Development** - Rill is functional but not yet production-ready.

**Complete:**

- ✅ Grammar specification (ABNF)
- ✅ Full parser with comprehensive tests
- ✅ AST and type definitions
- ✅ Virtual machine core
- ✅ Heap tracking system
- ✅ Builtin registry

**In Progress:**

- 🚧 IR lowering (AST → IR)
- 🚧 Standard library modules

**Planned:**

- ⏳ Optimizer passes
- ⏳ CBOR encode/decode integration
- ⏳ Compiled bytecode format
- ⏳ Comprehensive standard library

## Documentation

- **[DESIGN.md](docs/DESIGN.md)**: Comprehensive design document (architecture, rationale, implementation details)
- **[STDLIB.md](docs/STDLIB.md)**: Standard library documentation
- **[grammar.abnf](docs/grammar.abnf)**: Formal ABNF grammar specification
- **[example.txt](docs/example.txt)**: Extensive syntax examples

## Use Cases

While born in the Deep Space Network, Rill is a general-purpose language suitable for:

- **CBOR document validation and transformation**: Native CBOR support for policy enforcement
- **Embedded scripting**: Safe sandboxing for untrusted scripts in Rust applications
- **Configuration processing**: Complex rule evaluation without compromising host safety
- **Data transformation pipelines**: Rich pattern matching for structured data manipulation
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

Rill is in early development and feedback is greatly appreciated! Areas of particular interest:

- **IR lowering completion**: Help finish the AST → IR transformation
- **Standard library**: Implementing core modules (time, encoding, parsing)
- **Optimizer**: Constant folding, dead code elimination, type narrowing
- **Documentation**: Examples, tutorials, API documentation
- **Testing**: Edge cases, fuzzing, integration tests

Please open issues for bugs, feature requests, or design discussions.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

## Acknowledgments

- Inspired by the needs of [Hardy](https://github.com/YOUR_USERNAME/hardy) and the Deep Space Network
- Built with [chumsky](https://github.com/zesterer/chumsky) parser combinators
- Designed for environments where reliability isn't optional

---

*"The oldest minerals on Earth, chemically inert and fundamentally hardy—built to last."*
