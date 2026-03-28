# Rill Function Library

See the **Terminology** section in `DESIGN.md` for definitions of core,
prelude, stdlib, and externs.

## Prelude (Injected Source)

Prelude functions are Rill source text injected at the start of every
program. They are regular user-defined functions — not core intrinsics,
not externs. In bytecode, they appear in the function list alongside user
code.

### Existence Checking
- `is_defined(x)` - Returns `Bool` (true if present, false if undefined)

### Type Checking (is_)
- `is_uint(x)`, `is_int(x)`, `is_float(x)`, `is_bool(x)`
- `is_text(x)`, `is_bytes(x)`, `is_array(x)`, `is_map(x)`
- All return `Bool`, never undefined
- These compile to `Match` + Phi — identical to hand-written pattern matching

### Type Conversion (to_)
- `to_uint(x)`, `to_int(x)`, `to_float(x)`, `to_text(x)`
- Return a **new value** (converted), or undefined on failure
- Use with `if let`: `if let n = to_uint(val) { use(n); }`

### Utilities
- `default(value, fallback)` — returns value if defined, else fallback

### Core Intrinsics (Not Prelude)

These are hard-coded in the compiler, not prelude source:

- `len(x)` — Collection/sequence length (core intrinsic, callable by name)
- `collect(seq)` — Materialize sequence to array (core intrinsic, callable by name)

### Type Patterns

Type patterns are syntax, not functions:
- `with UInt(n) = value;` - n is reference if value is UInt, else undefined
- `if with UInt(n) = value { n += 1; }` - conditional reference binding
- `let UInt(n) = value;` - n is copy if value is UInt, else undefined

## Stdlib Modules (Registered by Embedder)

Stdlib modules are Rust crates providing utility functions via
`ExternRegistry`. The embedder opts in by registering them. In bytecode,
they appear as symbolic `FunctionRef` names resolved at load time.

### Domain-Specific Modules (DTN/Bundle Protocol)

The following modules are domain-specific examples for DTN bundle processing applications.
Host applications can provide their own domain-specific modules using the same patterns.

#### `std.status_report.codes`
Bundle Protocol and BPSec status report reason codes (RFC 9171, RFC 9172)

```rust
import std.status_report.codes as codes;

exit codes::LifetimeExpired;
exit codes::FailedSecurityOperation;
```

See `stdlib_example.txt` for all constants.

#### `std.bpsec`
BPSec signature and encryption validation

```rust
import std.bpsec;

if !bpsec::validate_signature(block, bundle) {
    exit codes::FailedSecurityOperation;
}
```

#### `std.admin`
Administrative bundle handling

```rust
import std.admin;

if admin::is_admin_record(bundle) {
    process_admin(bundle);
}
```

### General-Purpose Stdlib Modules

#### `std.cbor`
CBOR encoding/decoding utilities

```rust
import std.cbor;

if !cbor::is_well_formed(data) {
    exit codes::BlockUnintelligible;
}
```

#### `std.time`
Time and timestamp functions

```rust
import std.time;

let now = time::now();
let formatted = time::format_rfc3339(timestamp);
```

#### `std.parsing`
String parsing functions (beyond prelude)

```rust
import std.parsing;

// parse_int returns a value, use if let (no ? needed)
if let value = parsing::parse_int(text) {
    use(value);
}
```

#### `std.encoding`
Encoding/decoding utilities

```rust
import std.encoding;

let hex = encoding::hex_encode(bytes);
let b64 = encoding::base64_encode(bytes);
```

## Module System

See `DESIGN.md` Module System section for full details.

### Importing (Source Modules)
```rill
// Dotted paths (resolve to Rill source modules)
import std.bpsec;
import std.status_report.codes as codes;

// Local files (quoted strings)
import "../common/validation.rill";
import "./helpers.rill" as helpers;
```

### Stdlib/Extern Access (No Import Needed)
```rill
// Stdlib and embedder-provided functions are accessed via namespace
// qualification — registered by the embedder, not imported
math::sqrt(x)
console::log("hello")
```

### Namespacing
```rill
// Source module functions use namespace from import
codes::LifetimeExpired
validation::check_structure(bundle)

// Prelude and core intrinsics need no namespace
len(array)
is_uint(value)
is_defined(value)
```

### Default Aliases
- Dotted paths: last component (`std.bpsec` → `bpsec`)
- Files: filename without extension (`"helpers.rill"` → `helpers`)
- Override with `as name`

## Design Principles

1. **Prelude for essentials** — common functions always available, as Rill source
2. **Explicit opt-in** — stdlib and domain modules require embedder registration
3. **Consistent semantics** — failed operations return undefined, not exceptions
4. **No magic** — prelude is just source code; core intrinsics are minimal
5. **Duck typing** — type checking is runtime, not compile-time
