# Standard Library Organization

## Prelude (Auto-imported)

These functions are available in every script without explicit import.

### Type Intrinsics

| Function | Returns | Purpose |
|----------|---------|---------|
| `is_X()` | `Bool` | Type check (sugar for pattern match) |
| `to_X()` | `X` or `missing` | Type conversion (creates new value) |

Note: Type patterns (`UInt(x)`, `Text(s)`, etc.) replace the old `as_X()` functions.
Use `with Type(x) = value` for type-narrowing reference bindings.

### Size/Length
- `len(x)` - Returns length of Array, Map, Text, or Bytes

### Existence Checking
- `is_some(x)` - Returns `Bool` (true if present, false if missing)

### Type Checking (is_) - Compiler Sugar
- `is_uint(x)`, `is_int(x)`, `is_float(x)`, `is_bool(x)`
- `is_text(x)`, `is_bytes(x)`, `is_array(x)`, `is_map(x)`
- All return `Bool`, never `missing`
- **These are pure syntactic sugar** - compiler lowers to pattern matching:
  - `is_uint(x)` → `if let UInt(_) = x { true } else { false }`
- Kept for convenience; internally everything is pattern matching

### Type Patterns (replaces as_X)
- Use type patterns for type-narrowing bindings:
  - `with UInt(n) = value;` - n is reference if value is UInt, else missing
  - `if with UInt(n) = value { n += 1; }` - conditional reference binding
  - `let UInt(n) = value;` - n is copy if value is UInt, else missing

### Type Conversion (to_)
- `to_uint(x)`, `to_int(x)`, `to_float(x)`, `to_text(x)`
- Return a **new value** (converted), or `missing` on failure
- Use with `if let`: `if let n = to_uint(val) { use(n); }`
- No `?` needed: the implicit presence check IS the point of if let

See `stdlib_prelude.txt` for detailed documentation.

## Core Modules (Explicit Import)

### Domain-Specific Modules (DTN/Bundle Protocol)

The following modules are domain-specific examples for DTN bundle processing applications.
Host applications can provide their own domain-specific modules using the same patterns.

#### `std.status_report.codes`
Bundle Protocol and BPSec status report reason codes (RFC 9171, RFC 9172)

```rust
import std.status_report.codes as codes;

drop codes::LifetimeExpired;
drop codes::FailedSecurityOperation;
```

See `stdlib_example.txt` for all constants.

#### `std.bpsec`
BPSec signature and encryption validation

```rust
import std.bpsec;

if !bpsec::validate_signature(block, bundle) {
    drop codes::FailedSecurityOperation;
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

### General-Purpose Modules

#### `std.cbor`
CBOR encoding/decoding utilities

```rust
import std.cbor;

if !cbor::is_well_formed(data) {
    drop codes::BlockUnintelligible;
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

### Importing
```rust
// Standard library (unquoted dotted paths)
import std.bpsec;
import std.status_report.codes as codes;

// Local files (quoted strings)
import "../common/validation.flt";
import "./helpers.flt" as helpers;
```

### Namespacing
```rust
// Call imported functions with namespace::function
codes::LifetimeExpired
bpsec::validate_signature(block, bundle)
validation::check_structure(bundle)

// Prelude functions don't need namespace
len(array)
is_uint(value)
```

### Default Aliases
- Standard library: last component (`std.bpsec` → `bpsec`)
- Files: filename without extension (`"helpers.flt"` → `helpers`)
- Override with `as name`

## Design Principles

1. **Auto-import essentials** - Core operations always available
2. **Explicit opt-in** - Specialized functionality requires import
3. **Consistent semantics** - All coercion returns `missing` on failure
4. **No magic** - Clear distinction between prelude and imports
5. **Duck typing** - Type checking is runtime, not compile-time
