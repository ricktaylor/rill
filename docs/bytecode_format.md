# Rill Bytecode Format

## Status

Draft design document. Not yet implemented.

## Motivation

Rill is an embedded scripting language. The compilation pipeline is:

```
Source -> AST -> IR (SSA) -> Optimize -> Compile (closures) -> Execute
```

The `compile` step generates Rust closures that capture resolved function
pointers, slot offsets, and constant values. These closures are the runtime
VM opcode format. They cannot be serialized — they contain native pointers
and heap references that are specific to the embedding process.

For rapid loading, we need a bytecode format that:

1. **Serializes optimized IR** — the representation before closure compilation
2. **Is fully linked** — all source-level imports resolved; functions and
   constants from every source file merged into one compilation unit
3. **Supports late binding** — the only unresolved symbols are extern
   functions (stdlib and embedder-provided), referenced by name and resolved
   against the host's `ExternRegistry` at load time
4. **Allows re-optimization** — the optimizer can run again after loading,
   since the host's externs may differ from the original compilation context
5. **Carries optional debug info** — source spans for optimizer warnings,
   separable from the core bytecode (like DWARF in ELF)

The load path is:

```
Bytecode -> deserialize -> IrProgram
         -> ir::opt::optimize(&program, &externs)
         -> compile::compile_program(&program, &externs)
         -> CompiledProgram (closures, ready to execute)
```

This is identical to source compilation, skipping parse, lower, and
import resolution.

## Wire Format: CBOR via hardy-cbor

### Why CBOR

- **Binary and compact.** Integer opcodes, varint lengths, no field names on
  the wire. Most instructions encode in 4-8 bytes.
- **Self-describing.** Bytecode files can be inspected with generic CBOR tools
  (including the hardy cbor CLI tools already in the workspace).
- **Forward-compatible.** CBOR maps with integer keys allow new fields to be
  added without breaking older readers — unknown keys are skipped.
- **Canonical encoding.** Hardy-cbor produces deterministic shortest-form
  output. Identical programs produce identical bytecodes, enabling caching
  and integrity checks.
- **No new dependencies.** Hardy-cbor is already in the workspace, has minimal
  transitive deps (`thiserror`, `half`), and is no-std compatible.

### Why not serde

- **Format stability.** Serde derive ties wire format to Rust struct layout.
  Renaming a field, reordering an enum variant, or adding a variant silently
  changes the serialized output. For a bytecode format, the encoding is the
  ABI contract and must be explicitly controlled.
- **Compact control.** Manual `ToCbor`/`FromCbor` impls let us encode
  `IntrinsicOp::Add` as integer `0` (1 byte) rather than string `"Add"`
  (4 bytes), and `VarId(3)` as bare integer `3` rather than a tagged struct.
- **Dependency cost.** Serde plus a binary format crate (postcard, bincode)
  adds two or more dependencies to Rill, which currently has only three
  (`thiserror`, `chumsky`, `indexmap`).

### Why not a custom binary format

- CBOR gives us self-description, forward compatibility, and canonical form
  for free. A custom format would need to reimplement all of these.
- Existing tooling (RFC 8949 ecosystem, hardy cbor CLI) works immediately.

## Top-Level Structure

### Bytecode is a fully-linked unit

A single source file is the root of a Rill program. Its functions and
constants are the public interface. Imports are resolved recursively
during lowering — imported functions and constants are folded into the
`IrProgram` as part of normal compilation. By the time the IR exists,
everything is already in one place.

Bytecode simply serializes that already-complete `IrProgram`. There is no
separate link or merge step — the compiler's import resolution has already
done the work.

### Three kinds of function reference in bytecode

Every `Instruction::Call` in bytecode contains a `FunctionRef` — a
symbolic name. At load time, `compile_program` resolves each reference
into one of three categories:

| Category | Example | Resolution | In bytecode |
|----------|---------|------------|-------------|
| **Core intrinsic** | `Add`, `MakeArray` | N/A — already an `IntrinsicOp` | Integer opcode in `Instruction::Intrinsic` |
| **Internal function** | `["my_func"]`, `["is_uint"]` | Matched against bytecode's own function list | Resolved at link time, no registry needed |
| **Extern** | `["math", "sqrt"]`, `["exit"]` | Looked up in host's `ExternRegistry` | Late-bound; `E500_UndefinedExternal` if missing |

Core intrinsics never appear as `Call` instructions — they are lowered to
`Instruction::Intrinsic` during compilation and encoded as integer opcodes.

Internal functions include both user-defined code and prelude functions.
They are present in the bytecode's function list and resolve without any
external registry.

Externs (stdlib and embedder-provided functions) are the only truly
late-bound symbols. They appear as symbolic `FunctionRef` names and are
resolved against the host's `ExternRegistry` at load time.

### Top-level structure

The bytecode file is a single CBOR map with integer keys:

```
{
  0: h'52494C4C',        ; magic: "RILL" as byte string
  1: <uint>,             ; format version (currently 1)
  2: [<Function>, ...],  ; functions (all source modules merged)
  3: [<ConstBinding>, ...], ; constants (all source modules merged)
  4: <DebugInfo>         ; optional debug info (key absent = no debug info)
}
```

Integer keys keep the top-level map compact and allow forward compatibility —
a reader encountering an unknown key (e.g., key `5` from a future version)
skips it gracefully.

## Type Encoding Reference

### Primitive Wrappers

These IR types are newtypes over integers. They encode as bare CBOR unsigned
integers with no wrapper:

| Type      | CBOR encoding | Example              |
|-----------|---------------|----------------------|
| `VarId`   | uint          | `VarId(3)` -> `3`    |
| `BlockId` | uint          | `BlockId(7)` -> `7`  |
| `TypeSet` | uint          | `TypeSet { bits: 0x07 }` -> `7` |

### BaseType

Encoded as a CBOR unsigned integer via `From<BaseType> for u64`:

| Variant    | Value |
|------------|-------|
| `Bool`     | 0     |
| `UInt`     | 1     |
| `Int`      | 2     |
| `Float`    | 3     |
| `Text`     | 4     |
| `Bytes`    | 5     |
| `Array`    | 6     |
| `Map`      | 7     |
| `Sequence` | 8     |

### Identifier

`ast::Identifier(String)` encodes as a CBOR text string.

## Literal Encoding

Encoded as a 2-element CBOR array `[type_tag, value]`:

| Variant        | Tag | Value encoding         | Example                     |
|----------------|-----|------------------------|-----------------------------|
| `Literal::Bool`  | 0   | CBOR bool            | `true` -> `[0, true]`      |
| `Literal::UInt`  | 1   | CBOR uint            | `42` -> `[1, 42]`          |
| `Literal::Int`   | 2   | CBOR int (neg or pos)| `-5` -> `[2, -5]`          |
| `Literal::Float` | 3   | CBOR float           | `3.14` -> `[3, 3.14]`      |
| `Literal::Text`  | 4   | CBOR text string     | `"hi"` -> `[4, "hi"]`      |
| `Literal::Bytes` | 5   | CBOR byte string     | `b"\x01\x02"` -> `[5, h'0102']` |

## ConstValue Encoding

Same tag scheme as Literal, extended with recursive collection types:

| Variant           | Tag | Value encoding                    |
|-------------------|-----|-----------------------------------|
| `ConstValue::Bool`  | 0   | CBOR bool                       |
| `ConstValue::UInt`  | 1   | CBOR uint                       |
| `ConstValue::Int`   | 2   | CBOR int                        |
| `ConstValue::Float` | 3   | CBOR float                      |
| `ConstValue::Text`  | 4   | CBOR text string                |
| `ConstValue::Bytes` | 5   | CBOR byte string                |
| `ConstValue::Array` | 6   | CBOR array of ConstValue        |
| `ConstValue::Map`   | 7   | CBOR array of [key, value] pairs|

`ConstValue::Map` encodes as `[7, [[key, value], [key, value], ...]]` to
preserve insertion order (CBOR maps do not guarantee order).

## IntrinsicOp Encoding

Encoded as a CBOR unsigned integer:

| Variant    | Value | | Variant   | Value |
|------------|-------|-|-----------|-------|
| `Add`      | 0     | | `BitNot`  | 13    |
| `Sub`      | 1     | | `Shl`     | 14    |
| `Mul`      | 2     | | `Shr`     | 15    |
| `Div`      | 3     | | `BitTest` | 16    |
| `Mod`      | 4     | | `BitSet`  | 17    |
| `Neg`      | 5     | | `Len`     | 18    |
| `Eq`       | 6     | | `MakeArray` | 19  |
| `Lt`       | 7     | | `MakeMap` | 20    |
| `Not`      | 8     | | `MakeSeq` | 21   |
| `BitAnd`   | 9     | | `ArraySeq`| 22   |
| `BitOr`    | 10    | | `SeqNext` | 23   |
| `BitXor`   | 11    | | `Collect` | 24   |
| `Widen`    | 12    | | `Cast`    | 25   |

These values are defined in the `From<IntrinsicOp> for u64` impl. New
intrinsics are appended at the end; existing values never change.

## Instruction Encoding

Each instruction is a CBOR array. The first element is an integer opcode.
Remaining elements are the instruction's operands.

| Opcode | Instruction   | Encoding                                      |
|--------|---------------|-----------------------------------------------|
| 0      | `Phi`         | `[0, dest, [[block, var], ...]]`              |
| 1      | `Copy`        | `[1, dest, src]`                              |
| 2      | `Undefined`   | `[2, dest]`                                   |
| 3      | `Const`       | `[3, dest, <Literal>]`                        |
| 4      | `Index`       | `[4, dest, base, key]`                        |
| 5      | `SetIndex`    | `[5, base, key, value]`                       |
| 6      | `Intrinsic`   | `[6, dest, op, [args...]]`                    |
| 7      | `Call`        | `[7, dest, <FunctionRef>, [<CallArg>, ...]]`  |
| 8      | `MakeRef`     | `[8, dest, base, key_or_null]`                |
| 9      | `WriteRef`    | `[9, ref_var, value]`                         |
| 10     | `Drop`        | `[10, [vars...]]`                             |

### FunctionRef

Encoded as a CBOR array:

```
FunctionRef { namespace: None, name: "foo" }    -> ["foo"]
FunctionRef { namespace: Some("str"), name: "len" } -> ["str", "len"]
```

Single-element array = unqualified; two-element array = qualified. This
avoids encoding `null` for the common unqualified case.

### CallArg

Encoded as a CBOR array:

```
CallArg { value: VarId(3), by_ref: false } -> [3]
CallArg { value: VarId(3), by_ref: true }  -> [3, true]
```

Single-element = by-value (common case); two-element = by-ref. The `by_ref`
flag is only emitted when true, saving a byte per argument in the common case.

### Const Instruction Detail

The Literal is inlined (not wrapped in an extra array):

```
Instruction::Const { dest: VarId(3), value: Literal::UInt(42) }
-> [3, 3, 1, 42]
    ^  ^  ^   ^
    |  |  |   +-- literal value
    |  |  +------ literal type tag (UInt=1)
    |  +--------- dest
    +------------ opcode (Const=3)
```

The literal's `[type, value]` pair is flattened into the instruction array
rather than nested, avoiding one level of CBOR array overhead.

## Terminator Encoding

Each terminator is a CBOR array with an integer opcode:

| Opcode | Terminator    | Encoding                                     |
|--------|---------------|----------------------------------------------|
| 0      | `Jump`        | `[0, target]`                                |
| 1      | `If`          | `[1, condition, then_target, else_target]`   |
| 2      | `Match`       | `[2, value, [<MatchArm>, ...], default]`     |
| 3      | `Guard`       | `[3, value, defined, undefined]`             |
| 4      | `Return`      | `[4]` or `[4, var]`                          |
| 5      | `Exit`        | `[5, var]`                                   |
| 6      | `Unreachable` | `[6]`                                        |

### MatchPattern

Encoded as a CBOR array:

```
MatchPattern::Literal(lit) -> [0, <Literal>]
MatchPattern::Type(ty)     -> [1, <BaseType>]
MatchPattern::Array(n)     -> [2, n]
MatchPattern::ArrayMin(n)  -> [3, n]
```

### Match Arm

A match arm is a 2-element array: `[<MatchPattern>, target_block]`.

### Return

Variable-length encoding:

```
Terminator::Return { value: None }          -> [4]
Terminator::Return { value: Some(VarId(2)) } -> [4, 2]
```

## Var Encoding

```
Var { id: VarId(0), name: "x", type_set: TypeSet::all() }
-> [0, "x", 511]
    ^   ^    ^
    |   |    +-- type_set bits as uint
    |   +------- name
    +----------- id
```

## Param Encoding

```
Param { var: VarId(1), by_ref: false } -> [1]
Param { var: VarId(1), by_ref: true }  -> [1, true]
```

Same optimization as CallArg — omit `by_ref` when false.

## BasicBlock Encoding

```
BasicBlock {
  id: BlockId(0),
  instructions: [...],
  terminator: ...
}
-> [0, [<Instruction>, ...], <Terminator>]
```

## Function Encoding

CBOR map with integer keys for forward compatibility:

```
{
  0: "function_name",       ; name
  1: [<Param>, ...],        ; params
  2: <Param> or null,       ; rest_param (null if absent)
  3: [<Var>, ...],          ; locals
  4: [<BasicBlock>, ...],   ; blocks
  5: <uint>                 ; entry_block id
}
```

## ConstBinding Encoding

```
ConstBinding { name: "MAX_TTL", value: ConstValue::UInt(86400) }
-> ["MAX_TTL", 1, 86400]
     ^          ^   ^
     |          |   +-- value
     |          +------ type tag
     +----------------- name
```

The ConstValue `[type, value]` pair is flattened, same as Literal in Const
instructions.

## Debug Info

Debug info occupies key `4` in the top-level map. When absent, all spans
reconstruct as `Span::default()` (zero-length at offset 0).

### Structure

```
{
  0: "source.rill",                ; source filename
  1: "fn test(x) { ... }",        ; source text (optional, key absent = not included)
  2: [                             ; per-function debug tables
    [                              ;   function 0
      [<uint>, <uint>],            ;     function-level span [start, end]
      [                            ;     per-block span tables
        [[s,e], [s,e], [s,e]],     ;       block 0: one [start, end] per instruction
        [[s,e], [s,e]],            ;       block 1
        ...
      ]
    ],
    ...                            ;   function 1, 2, ...
  ]
}
```

### Addressing

A span for instruction `i` in block `b` of function `f` is at:

```
debug_info[2][f][1][b][i] -> [start_offset, end_offset]
```

### Rationale

Separating debug info from the core bytecode serves several purposes:

- **Production deployments** can strip debug info entirely (omit key `5`),
  reducing bytecode size. The core IR is fully functional without it.
- **Optimizer warnings** need to map IR locations back to source. When debug
  info is present, the optimizer loads it, emits warnings with source
  positions, then discards it.
- **Parallel arrays** (one span per instruction, indexed by position) avoid
  polluting every instruction encoding with two extra integers. This mirrors
  how DWARF separates line tables from code in ELF.
- **Source text inclusion** is optional. When present, error messages can
  display the relevant source line with a caret. When absent, only
  `filename:offset` is reported.

### What debug info does NOT cover

Rill has no runtime exceptions or stack traces. All type errors produce
`undefined` (duck typing), arithmetic overflow produces `undefined`, and
out-of-bounds access produces `undefined`. The only runtime errors are
`StackOverflow` and `HeapOverflow`, which are VM-level faults that don't
reference source positions. Therefore, debug info is only needed during
the optimization and link phases, never at execution time.

## Format Evolution

### Version field

Key `1` in the top-level map is the format version. The current version
is `1`. A reader must reject bytecode with a version it does not understand.

### Forward compatibility rules

Within a version:

1. **New top-level keys** may be added. Readers skip unknown keys.
2. **New instruction opcodes** may be added at the end of the opcode table.
   A reader encountering an unknown opcode must reject the bytecode (it
   cannot safely skip instructions without understanding their semantics).
3. **New intrinsic ops** may be added at the end of the intrinsic table.
   Same rule: unknown intrinsics cause rejection.
4. **New terminator opcodes** may be added at the end. Same rule.
5. **Existing encodings never change.** Once assigned, an opcode or
   discriminant value is permanent.

A major version bump (e.g., v1 -> v2) is required for:

- Changing the encoding of an existing opcode
- Removing or renumbering any discriminant
- Changing the top-level structure

### Compatibility with extern changes

Extern calls are encoded as symbolic `FunctionRef` names (e.g.,
`["math", "sqrt"]`). At load time, `compile_program` resolves each
`FunctionRef` — first against the bytecode's own function list (internal
functions including prelude), then against the host's `ExternRegistry`
(stdlib and embedder-provided externs). If a reference doesn't resolve
in either:

- The linker emits an `E500_UndefinedExternal` diagnostic
- No silent breakage — missing functions are caught at load time, not at
  runtime

This is the same behaviour as loading from source.

## Two-Phase Compilation Pipeline

### Problem

The current `optimize()` function takes `&ExternRegistry` as a required
parameter. But for bytecode emission, we want to optimize *before* externs
are known — the whole point is that bytecode is compiled once and loaded
into different embeddings with different extern registries.

### Extern Dependencies by Pass

The optimizer refers to externs via the `ExternRegistry` type (the Rust
API for registering stdlib and embedder-provided functions). In the
analysis below, "externs" refers to this registry — i.e., extern
functions, not core intrinsics or prelude functions.

| Pass | Externs? | What it uses | Works without? |
|------|-----------|--------------|----------------|
| `const_fold` | **Required** | `eval_extern_const()`: looks up extern by name, checks `Purity::Const`, calls const evaluator | Partially — intrinsic folding works, extern call folding doesn't |
| `type_refinement` | Optional | `extern.meta.returns.type_sig()` for return type narrowing; `extern.meta.diverges()` for empty TypeSet | YES — falls back to `TypeSet::all()` for unknown calls |
| `dce` | Optional | `extern.meta.purity.is_pure()` to determine if Call is removable | YES — conservatively keeps all extern calls |
| `cse` | Optional | `extern.meta.purity.is_pure()` to determine if Call is CSE-able | YES — conservatively skips extern calls |
| `definedness` | Optional | `extern.meta.diverges()`, `extern.meta.purity.may_return_undefined()` for call result definedness | YES — conservatively returns `MaybeDefined` for all extern calls |
| `guard_elim` | None | Uses `DefinednessAnalysis` output only | YES |
| `copy_prop` | None | Pure structural rewrite | YES |
| `algebra` | None | Uses `TypeAnalysis` only | YES |
| `cast_elision` | None | Uses `TypeAnalysis` only | YES |
| `coercion` | None | Uses `TypeAnalysis` only | YES |
| `ref_elision` | None | Pure dataflow analysis | YES |

**Key finding:** Only `const_fold` has a hard dependency, and even that is
limited to folding calls to `Purity::Const` externs. Intrinsic constant
folding (the vast majority — arithmetic, comparison, bitwise on literals)
works without externs. All other passes either don't use externs or
already handle `Option<&ExternRegistry>` with `None`.

### Proposed Split: `optimize` vs `optimize_pre_link`

```rust
/// Optimize without externs — for bytecode emission.
///
/// Runs all passes that don't require extern metadata.
/// Constant folding still folds intrinsics (Add, Mul, etc.) on literal
/// operands. Only extern-call folding is deferred to post-link.
///
/// Interprocedural passes (monomorphization, param propagation, return
/// type inference) run with externs=None, which means:
/// - Extern calls get TypeSet::all() return types (conservative)
/// - Extern calls are conservatively non-pure (not CSE'd or DCE'd)
/// - Extern calls with Purity::Const are NOT folded
///
/// These are all correct — just less optimized. The post-link pass
/// recovers the lost optimizations when externs are known.
pub fn optimize_pre_link(
    program: &mut IrProgram,
    diagnostics: &mut Diagnostics,
);

/// Full optimization with externs — current behaviour.
///
/// Used for source compilation (externs known) and post-link
/// re-optimization after bytecode loading.
pub fn optimize(
    program: &mut IrProgram,
    externs: &ExternRegistry,
    diagnostics: &mut Diagnostics,
);
```

### What `optimize_pre_link` runs

Phase 1 (intraprocedural fixpoint) — all passes work without externs:

1. `fold_constants(function, &ExternRegistry::new(), diagnostics)` —
   intrinsic folding works; extern call folding is a no-op (empty registry)
2. `eliminate_common_subexpressions(function)` — no externs needed
3. `propagate_copies(function)` — no externs needed
4. `eliminate_dead_code(function)` — no externs needed (conservative on calls)
5. `elide_refs(function)` — no externs needed
6. `elide_coercions(function)` — no externs needed
7. `analyze_definedness(function, None)` — handles None
8. `check_definedness(function, &defs, None, diagnostics)` — handles None
9. `eliminate_guards(function, &defs)` — no externs needed
10. `simplify_cfg(function)` — no externs needed

Phase 2 (type-informed) — all passes work without externs:

1. `analyze_types(function, None)` — handles None (unknown call returns = all)
2. `check_intrinsic_types(function, &types, diagnostics)` — no externs needed
3. `check_condition_types(function, &types, diagnostics)` — no externs needed
4. `insert_coercions(function, &types)` — no externs needed
5. `elide_identity_casts(function, &types)` — no externs needed
6. `simplify_algebra(function, &types)` — no externs needed
7. `fold_non_bool_conditions(function, &types)` — no externs needed
8. `eliminate_dead_match_arms(function, &types)` — no externs needed

Phase M (interprocedural) — works with externs=None:

1. `monomorphize(program, &ExternRegistry::new())` — type analysis falls
   back to all() for extern call return types; monomorphization still
   works for user function calls (which is the primary use case)
2. `collect_param_info(program, None)` — handles None
3. `collect_pure_functions(program, None)` — conservatively marks all
   functions calling externs as impure
4. Return type inference — handles None
5. Phase B3 re-optimization — same as Phase 1+2 above

### What the post-link pass recovers

When bytecode is loaded and `optimize()` runs with the actual externs:

1. **Const folding of extern calls.** E.g., `math::sqrt(4.0)` with
   `Purity::Const` folds to `2.0`. This couldn't happen pre-link because
   `math::sqrt` wasn't in the registry.

2. **Tighter return types.** `str::upper(x)` returns `TypeSet::text()`,
   not `TypeSet::all()`. This enables downstream type narrowing, coercion
   elimination, and match arm pruning that couldn't fire pre-link.

3. **Extern purity for DCE/CSE.** Pure extern calls with unused results
   are eliminated. Identical pure extern calls are merged. Pre-link,
   these calls were conservatively kept.

4. **Divergence detection.** `exit()` with `ReturnBehavior::Exits` makes
   code after it unreachable. Pre-link, it was treated as a normal call.

### Impact assessment

For typical Rill programs, the pre-link optimizer will catch the vast
majority of optimizations:

- **Intrinsic folding** (the bulk of constant folding): `2 + 3` -> `5`,
  `true && false` -> `false`, etc. All work without externs.
- **Type narrowing** for user-defined functions: fully functional.
- **Guard/branch elimination** from definedness analysis: fully functional.
- **Copy propagation, DCE, CSE** on intrinsics: fully functional.
- **Algebraic simplification**: fully functional.
- **Coercion insertion/elimination**: fully functional.
- **Monomorphization** of user functions: fully functional.

The only optimizations deferred to post-link involve extern calls, which
are typically a small fraction of the IR. The post-link pass is fast
because it operates on already-optimized IR — most fixpoint loops converge
in one iteration since the intrinsic-level work is already done.

### Updated Compilation Pipelines

**Source compilation (current, unchanged):**

```
Source -> parse -> AST -> lower -> IR
      -> optimize(ir, externs, diags)        // full optimization
      -> compile_program(ir, externs)         // link + closures
      -> CompiledProgram
```

**Bytecode emission (new):**

```
Root source file -> parse -> AST -> lower (resolves imports) -> IrProgram
                -> optimize_pre_link(ir, diags)    // optimize without externs
                -> extract_debug_info(ir)          // capture spans
                -> bytecode::save(ir, debug_info)  // serialize
                -> bytes
```

**Bytecode loading (new):**

```
bytes -> bytecode::load(bytes)                 // deserialize
      -> (IR, debug_info)
      -> reattach_spans(ir, debug_info)        // restore spans for diagnostics
      -> optimize(ir, externs, diags)         // re-optimize with externs
      -> compile_program(ir, externs)         // link + closures
      -> CompiledProgram
```

### Implementation cost

The `optimize_pre_link` function is a subset of `optimize` — it calls the
same passes with `None` or an empty registry where externs are expected.
The existing pass signatures already support this (most take
`Option<&ExternRegistry>`). The only change needed is making
`fold_constants` accept `Option<&ExternRegistry>` instead of
`&ExternRegistry` — a one-line signature change since the extern
lookup already returns `Option`.

## Implementation Plan

### New files

- `src/bytecode.rs` — public API: `save(IrProgram, Option<DebugInfo>) -> Vec<u8>` and `load(&[u8]) -> Result<(IrProgram, Option<DebugInfo>), BytecodeError>`
- `src/bytecode/encode.rs` — `ToCbor` impls and `From<Enum> for u64` mappings
- `src/bytecode/decode.rs` — `FromCbor` impls and `TryFrom<u64> for Enum` mappings
- `src/bytecode/debug_info.rs` — `DebugInfo` type plus encode/decode

### Dependencies

Add `hardy-cbor` as a workspace dependency, gated behind a `bytecode`
feature flag:

```toml
[features]
default = []
bytecode = ["dep:hardy-cbor"]

[dependencies]
hardy-cbor = { path = "../hardy/cbor", optional = true }
```

### Type count

| Category | Types | Estimated lines (encode + decode) |
|----------|-------|-----------------------------------|
| Primitives (`VarId`, `BlockId`, `TypeSet`, `BaseType`) | 4 | ~80 |
| Values (`Literal`, `ConstValue`) | 2 | ~120 |
| Ops (`IntrinsicOp`, `MatchPattern`) | 2 | ~80 |
| Instructions (`Instruction`, `CallArg`, `FunctionRef`) | 3 | ~200 |
| Terminators (`Terminator`) | 1 | ~80 |
| Containers (`Var`, `Param`, `BasicBlock`) | 3 | ~80 |
| Top-level (`Function`, `IrProgram`, `ConstBinding`) | 3 | ~100 |
| Debug info | 1 | ~80 |
| **Total** | **~19** | **~820** |

### Discriminant mapping

Bytecode discriminants are mapped directly in `From`/`TryFrom` impls on
the existing enums — no companion types, no `#[repr(u8)]`, no constants
file. This follows the pattern used throughout the hardy codebase (e.g.,
`bpv7::status_report::ReasonCode`).

For each enum that needs a CBOR integer encoding:

1. `impl From<Enum> for u64` — match arms mapping variants to integers
2. `impl TryFrom<u64> for Enum` — reverse mapping, returns error for unknown values
3. `ToCbor` — one-liner: `encoder.emit(&u64::from(*self))`
4. `FromCbor` — one-liner: decode `u64`, call `try_into()`

Example for `BaseType`:

```rust
impl From<BaseType> for u64 {
    fn from(value: BaseType) -> Self {
        match value {
            BaseType::Bool     => 0,
            BaseType::UInt     => 1,
            BaseType::Int      => 2,
            BaseType::Float    => 3,
            BaseType::Text     => 4,
            BaseType::Bytes    => 5,
            BaseType::Array    => 6,
            BaseType::Map      => 7,
            BaseType::Sequence => 8,
        }
    }
}

impl TryFrom<u64> for BaseType {
    type Error = BytecodeError;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(BaseType::Bool),
            1 => Ok(BaseType::UInt),
            // ...
            v => Err(BytecodeError::UnknownDiscriminant("BaseType", v)),
        }
    }
}

impl ToCbor for BaseType {
    type Result = ();
    fn to_cbor(&self, encoder: &mut Encoder) {
        encoder.emit(&u64::from(*self));
    }
}
```

`IntrinsicOp`, `Instruction`, `Terminator`, `Literal`, `MatchPattern`,
and `ConstValue` all follow the same pattern. For data-carrying enums,
the `ToCbor` match emits the discriminant then the variant's fields;
`FromCbor` reads the discriminant, matches, then reads the fields.

The discriminant values in the `From` impl ARE the bytecode ABI. New
variants are appended at the end; existing values never change.
Exhaustive matching in `From` ensures the compiler catches any new
variant added without a discriminant assignment.

### DebugInfo extraction

A helper function extracts debug info from an `IrProgram` before encoding:

```rust
pub struct DebugInfo {
    pub source_file: Option<String>,
    pub source_text: Option<String>,
    pub functions: Vec<FunctionDebug>,
}

pub struct FunctionDebug {
    pub span: (usize, usize),
    pub blocks: Vec<Vec<(usize, usize)>>,
}
```

The lowerer would need a small addition to record function-level spans.
Block/instruction spans are already present in `SpannedInst`.

### Public API

```rust
// bytecode.rs

/// Encode an optimized IR program to bytecode.
///
/// The program must be fully linked — all source-level imports resolved,
/// all user functions and constants present. The only unresolved symbols
/// should be FunctionRef names for host-provided externs.
///
/// If `debug_info` is provided, it is included in the bytecode file.
/// Pass `None` to produce a stripped bytecode without source mapping.
pub fn save(program: &IrProgram, debug: Option<&DebugInfo>) -> Vec<u8>;

/// Decode bytecode back to an IR program.
///
/// Returns the program and optional debug info (if present in the file).
/// The program can be passed to `compile_program()` with a `ExternRegistry`
/// to produce executable closures.
pub fn load(data: &[u8]) -> Result<(IrProgram, Option<DebugInfo>), BytecodeError>;

/// Extract debug info from a span-annotated IR program.
///
/// Call this before `save()` to capture source locations.
/// The returned `DebugInfo` is a parallel structure — same indices as the
/// functions and blocks in the `IrProgram`.
pub fn extract_debug_info(
    program: &IrProgram,
    source_file: Option<&str>,
    source_text: Option<&str>,
) -> DebugInfo;

/// Strip debug info from bytecode, returning smaller bytecode.
///
/// Operates directly on the CBOR wire format — parses the top-level map,
/// drops key 4 (debug info), re-emits. Does not deserialize any IR types,
/// so this works without understanding the IR schema and will remain
/// compatible across bytecode versions.
///
/// Returns the input unchanged if debug info is already absent.
pub fn strip(data: &[u8]) -> Result<Vec<u8>, BytecodeError>;
```

The `strip` function is intentionally CBOR-level, not IR-level. It
re-emits the top-level map with key `4` omitted, copying all other keys
as raw CBOR bytes (using hardy-cbor's `Raw` wrapper). This means:

- **No IR dependency.** The strip tool doesn't need to understand
  `IrProgram`, `Instruction`, or any IR types. It works on any valid
  Rill bytecode regardless of version.
- **Fast.** No deserialization/reserialization of the program — just a
  single pass copying raw bytes with one key filtered out.
- **Standalone tool.** Can be shipped as a tiny binary that depends only
  on hardy-cbor, not on the rill crate.

## Size Estimates

For a typical Rill program with 10 functions averaging 20 instructions each:

- ~200 instructions at ~6 bytes each = ~1,200 bytes
- ~50 terminators at ~4 bytes each = ~200 bytes
- ~100 variables at ~10 bytes each = ~1,000 bytes
- Function/program overhead = ~200 bytes
- **Total: ~2.5 KB** (without debug info)

Debug info adds roughly 4 bytes per instruction (two u16 offsets packed into
a CBOR array), so ~1 KB for the same program. Total with debug info: ~3.5 KB.

For comparison, the source text for the same program would be ~2-4 KB, so
bytecode is roughly equivalent in size but loads in microseconds (no parsing,
no lowering).
