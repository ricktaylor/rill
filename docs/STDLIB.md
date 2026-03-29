# Rill Function Library

See the **Terminology** section in `DESIGN.md` for definitions of core,
standard prelude, and externs.

## Standard Prelude

The rill crate provides `STANDARD_PRELUDE` — Rill source code containing
common utility functions. The embedder includes it via the `SourceLoader`
trait's `preamble()` method — see `DESIGN.md` for the full API. These are
regular Rill functions compiled alongside user code — not intrinsics, not
externs. The embedder can skip, customize, or extend the preamble.
Duplicate names between preamble and user code are a compile error — the
embedder must customise the preamble to resolve conflicts.

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

These are hard-coded in the compiler, not prelude functions:

- `len(x)` — Collection/sequence length (core intrinsic, callable by name)
- `collect(seq)` — Materialize sequence to array (core intrinsic, callable by name)

### Type Patterns

Type patterns are syntax, not functions:
- `with UInt(n) = value;` - n is reference if value is UInt, else undefined
- `if with UInt(n) = value { n += 1; }` - conditional reference binding
- `let UInt(n) = value;` - n is copy if value is UInt, else undefined

## Extern Namespaces (Registered by Embedder)

Extern namespaces are groups of Rust functions registered by the embedder
via `ExternRegistry::register_in()`. Scripts declare their dependency on
an extern namespace with `require`. In bytecode, they appear as symbolic
`FunctionRef` names resolved at load time.

### Domain-Specific Namespaces (DTN/Bundle Protocol)

The following are domain-specific examples for DTN bundle processing
applications. Host applications provide their own namespaces using the
same `register_in()` API.

#### `codes`
Bundle Protocol and BPSec status report reason codes (RFC 9171, RFC 9172)

```rill
require codes;

exit codes::LifetimeExpired;
exit codes::FailedSecurityOperation;
```


#### `bpsec`
BPSec signature and encryption validation

```rill
require bpsec;

if !bpsec::validate_signature(block, bundle) {
    exit codes::FailedSecurityOperation;
}
```

#### `admin`
Administrative bundle handling

```rill
require admin;

if admin::is_admin_record(bundle) {
    process_admin(bundle);
}
```

### General-Purpose Extern Namespaces

#### `cbor`
CBOR encoding/decoding utilities

```rill
require cbor;

if !cbor::is_well_formed(data) {
    exit codes::BlockUnintelligible;
}
```

#### `time`
Time and timestamp functions

```rill
require time;

let now = time::now();
let formatted = time::format_rfc3339(timestamp);
```

#### `parsing`
String parsing functions (beyond prelude)

```rill
require parsing;

// parse_int returns a value, use if let (no ? needed)
if let value = parsing::parse_int(text) {
    use(value);
}
```

#### `encoding`
Encoding/decoding utilities

```rill
require encoding;

let hex = encoding::hex_encode(bytes);
let b64 = encoding::base64_encode(bytes);
```

## Module System

See `DESIGN.md` Module System section for full details.

### Source File Imports
```rill
import "../common/validation.rill";     // Namespace: validation
import "./helpers.rill" as h;           // Namespace: h (explicit alias)
import "./utils.rill" as _;            // No namespace — functions available unqualified
```

### Extern Dependencies
```rill
require cbor;                           // Embedder must provide "cbor" namespace
require cbor as c;                      // Alias to "c"
require encoding as _;                  // No namespace — functions available unqualified
```

### Namespacing
```rill
// Qualified: extern and imported functions use namespace prefix
cbor::decode(bytes)
validation::check_structure(bundle)

// Unqualified: `as _` imports, global externs, intrinsics, standard prelude
hex_encode(data)                        // from `require encoding as _`
my_util(x)                             // from `import "utils.rill" as _`
len(array)                             // core intrinsic
is_uint(value)                         // standard prelude (if included)
exit(0)                                // global extern
```

### Aliases
- Source file imports: default is filename stem (`"helpers.rill"` → `helpers`)
- Extern requires: default is the namespace name (`require cbor` → `cbor`)
- `as name` overrides the namespace alias
- `as _` discards the namespace — functions merge into root scope

## Design Principles

1. **Standard prelude for essentials** — common functions as Rill source, embedder opt-in
2. **Explicit dependencies** — extern namespaces require `require` declarations
3. **Consistent semantics** — failed operations return undefined, not exceptions
4. **No magic** — prelude is embedder-provided source; core intrinsics are minimal
5. **Duck typing** — type checking is runtime, not compile-time
