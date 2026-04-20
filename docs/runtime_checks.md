# Runtime Checks Implicit in Exec Closures

This document captures the implicit checks inside each runtime exec function.
These are checks that could be lifted into the IR as explicit instructions,
enabling the optimizer to prove them and the peephole layer to fuse
`Guard/Match/If + Op` into a single specialized step.

**Legend:**
- **Guard(defined)** — check that a value is not `Value::Undefined`
- **Match(type)** — check that a value is a specific `BaseType` variant
- **If(cond)** — boolean condition check (bounds, overflow, etc.)
- **Convert** — implicit type coercion within the op

**Status column:**
- *emitted* — IR already emits this check (optimizer can reason about it)
- *direct* — `vm.local()` returns `&Value` directly, exec functions handle `Undefined` via `_ =>` arms
- *specialized* — `try_specialize_binary`/`try_specialize_convert` handles this
- **missing** — not represented in IR, runtime-only

**Runtime simplifications completed:**
- `Value::Undefined` replaces `Option<Value>` throughout — no more `None`/`Some` wrapping
- `vm.local()` returns `&Value` (not `Option<&Value>`) — no unwrap/expect needed
- `Slot::Uninit` removed — slots always contain a `Value` (including `Value::Undefined`)
- `set_local_uninit()` removed — use `set_local(d, Value::Undefined)` directly
- `all_defined` compiler flag removed — exec functions handle `Undefined` inputs naturally
- Public API returns `Result<Value>` not `Result<Option<Value>>`

---

## Arithmetic: exec_add, exec_sub, exec_mul, exec_div, exec_mod

All follow the same pattern with 9 type-pair match arms.

```
Guard(defined, a)                          — direct
Guard(defined, b)                          — direct
Match(a.type, b.type):
  (UInt, UInt)  → checked_op(a, b)         — specialized (try_specialize_binary)
  (Int, Int)    → checked_op(a, b)         — specialized
  (Float,Float) → float_op(a, b)           — specialized
  (UInt, Int)   → Convert(a, Int, Checked) + checked_op  — emitted (coercion pass)
  (Int, UInt)   → Convert(b, Int, Checked) + checked_op  — emitted (coercion pass)
  (UInt, Float) → Convert(a, Float) + float_op           — emitted (coercion pass)
  (Float,UInt)  → Convert(b, Float) + float_op           — emitted (coercion pass)
  (Int, Float)  → Convert(a, Float) + float_op           — emitted (coercion pass)
  (Float,Int)   → Convert(b, Float) + float_op           — emitted (coercion pass)
  _             → undefined                              — emitted (coercion: incompatible → Undefined)
If(overflow) → undefined                   — inherent in checked_op, not liftable

For div/mod only:
If(b != 0) → proceed                      — missing (could emit for integer div/mod)
  else → undefined
```

**Coverage: Good.** Coercion pass handles mixed types, `try_specialize_binary`
handles same-type, `all_defined` eliminates Guard. The only runtime dispatch
remaining is for genuinely unknown types (parameters without type info).

For `exec_div`/`exec_mod` specifically, a `b != 0` If guard before integer
division would let the optimizer prove the divisor is non-zero (e.g. from
a prior comparison or a known constant), enabling `checked_div` → `wrapping_div`
fusion in the peephole layer. Float division doesn't need this (div-by-zero
produces infinity, which Float::new rejects to undefined).

---

## exec_neg

```
Guard(defined, a)                          — direct
Match(a.type):
  Int   → checked_neg(a)                   — missing (no unary specialization)
  Float → float_neg(a)                     — missing
  UInt  → Convert(a, Int) + checked_neg    — missing (no coercion for unary)
  _     → undefined                        — missing
```

**Coverage: Weak.** No `try_specialize_unary` exists. The `all_defined` path
skips the Guard, but the type match is always at runtime. Unary type
specialization would be a small win.

---

## exec_eq

```
Guard(defined, a)                          — direct
Guard(defined, b)                          — direct
result = (a == b)                          — infallible, no type dispatch needed
```

**Coverage: Complete.** `Value` implements `PartialEq` across all types.
No type match needed. `all_defined` eliminates Guard.

---

## exec_lt

```
Guard(defined, a)                          — direct
Guard(defined, b)                          — direct
Match(a.type, b.type):
  same 9-pair matrix as arithmetic         — specialized / coercion pass
  _  → undefined                           — emitted (coercion: incompatible)
```

**Coverage: Good.** Same as arithmetic — coercion handles mixed types,
specialization handles same-type.

---

## exec_not

```
Guard(defined, a)                          — direct
Match(a.type):
  Bool → !a                                — missing (no unary type specialization)
  _    → undefined                         — missing
```

**Coverage: Weak.** Same as `exec_neg` — no unary specialization. The type
check `Match(Bool)` is always at runtime.

---

## Bitwise: exec_bitand, exec_bitor, exec_bitxor, exec_bitnot

```
Guard(defined, a)                          — direct
Guard(defined, b)                          — direct (binary only)
Match(a.type[, b.type]):
  (UInt[, UInt]) → bitwise_op(a, b)        — missing (no UInt specialization)
  _              → undefined               — missing
```

**Coverage: Weak.** `param_type` constrains to `TypeSet::uint()`, so the
type match never fails when types are correct. But no IR-level Match is
emitted — the check is purely defensive at runtime.

---

## exec_shl, exec_shr

Same as bitwise — `(UInt, UInt)` only, no IR-level type check.

---

## exec_bittest

```
Guard(defined, x)                          — direct
Guard(defined, b)                          — direct
Match(UInt, UInt)                          — missing
If(b < 64) → result                       — missing (bounds check)
  else → undefined
```

**Coverage: Weak.** The `b < 64` bounds check is runtime-only. Could be
an If guard in the IR when `b` is a known constant (const fold) or bounded
by a loop counter.

---

## exec_bitset

```
Guard(defined, x)                          — direct
Guard(defined, b)                          — direct
Guard(defined, v)                          — direct
Match(UInt, UInt, Bool)                    — missing
If(b < 64) → result                       — missing (bounds check)
  else → undefined
```

**Coverage: Weak.** Same as bittest — bounds check and type match are runtime-only.

---

## exec_len

```
Guard(defined, a)                          — direct
Match(a.type):
  Text | Bytes | Array | Map → len(a)     — missing (no collection type specialization)
  Sequence → remaining()                   — missing (may return None if unknown)
  _ → undefined                            — missing
```

**Coverage: Weak.** No type specialization for collection operations.
The `param_type` is `TypeSet::collection()` which is broad.

---

## exec_make_array

```
for each arg:
  Guard(defined, arg)                      — missing (filter_map silently drops undefined)
```

**Coverage: Acceptable.** Undefined elements are intentionally dropped
(sparse arrays aren't supported). This is semantic, not a missing check.

---

## exec_make_map

```
If(args.len() % 2 == 0) → proceed         — missing (but lowerer always emits even count)
for each (key, value) pair:
  Guard(defined, key)                      — missing (filter_map drops)
  Guard(defined, value)                    — missing (filter_map drops)
```

**Coverage: Acceptable.** Same as MakeArray — dropping undefined
key/value pairs is semantic behaviour.

---

## exec_make_seq

```
Guard(defined, start)                      — missing (could use all_defined)
Guard(defined, end)                        — missing (could use all_defined)
Match(UInt, start)                         — missing (param_type is UInt)
Match(UInt, end)                           — missing (param_type is UInt)
If(start < end) → MakeSeq                 — emitted (lowerer guard)
```

**Coverage: Partial.** The `start < end` guard is emitted. But definedness
and type checks on start/end are runtime-only. Since `param_type` is
`TypeSet::uint()`, the type mismatch diagnostic will catch non-UInt args
at compile time when types are known.

---

## exec_array_seq

```
Guard(defined, array)                      — missing
Guard(defined, start)                      — missing
Guard(defined, end)                        — missing
Match(Array, array)                        — missing
Match(UInt, start)                         — missing
Match(UInt, end)                           — missing
If(start < end) → ArraySeq                — emitted (lowerer guard)
```

**Coverage: Partial.** Same situation as MakeSeq. The `start < end` guard
is emitted. Type/definedness checks are runtime-only but the lowerer
produces these from known-type destructuring patterns, so args are
typically provably defined and correctly typed.

---

## exec_convert

```
Guard(defined, value)                      — specialized (try_specialize_convert)
Match(value.type):
  per (src, target, mode) combination      — specialized
```

**Coverage: Complete.** `try_specialize_convert` always fires (target and
mode are in the op variant). When source type is also known, the closure
is fully specialized with zero dispatch. The generic `exec_convert` path
is effectively unreachable.

---

## index_value (used by Index instruction)

```
Guard(defined, base)                       — partial (compile_instruction checks)
Guard(defined, key)                        — partial (compile_instruction checks)
Match(base.type, key.type):
  (Array, UInt)  → bounds check            — missing
  (Array, Int)   → sign check + bounds     — missing
  (Map, _)       → key lookup              — inherently fallible
  (Text, UInt)   → char-at                 — missing
  (Bytes, UInt)  → byte-at                 — missing
  _              → undefined               — missing
```

**Coverage: Weak.** The base/key definedness is checked at the call site
but not via `all_defined`. No type specialization for indexing. Bounds
checks are entirely runtime. This is the highest-impact target for
peephole fusion in loops (`arr[i]` with a loop guard `i < len(arr)`).

---

## Proposed Approach: Convert + Guard Type Insertion

The runtime closures contain implicit type dispatch (`match value { UInt(n) => ...,
_ => None }`) that is invisible to the IR. These can be made explicit using
existing IR primitives — **checked Convert + Guard** — without introducing
new instruction types.

### Principle

Every assumption the runtime makes should be visible in the IR as an instruction
the optimizer can reason about. The IR is the single source of truth for what's
been checked. The peephole layer is just pattern-matching to remove duplication
between what the IR proved and what the closure still defensively checks.

### Mechanism

A single pass, running after type refinement (Phase 2), inserts **Guard +
Match** sequences before intrinsic operations. This mirrors the Rust `match`
in each `exec_*` closure — making the same type dispatch explicit in the IR
so the optimizer can reason about it and the peephole layer can fuse it.

The Match instruction already exists in the IR (used for pattern matching).
Here it serves as a type guard: each arm dispatches to the type-specialized
operation, and the default arm produces Undefined.

**For numeric binary ops** (Add, Sub, Mul, Div, Mod, Lt):

```
// Before: exec_add has implicit 9-way type dispatch + _ → None
v2 = Intrinsic(Add, [v0, v1])

// After: explicit Match mirrors the runtime dispatch
Match(v0, v1):
  (UInt, UInt)   → uu_bb: v2 = Add(v0, v1)    // checked u64 add
  (Int, Int)     → ii_bb: v2 = Add(v0, v1)    // checked i64 add
  (Float, Float) → ff_bb: v2 = Add(v0, v1)    // f64 add
  // Mixed pairs handled by coercion pass (Convert inserted before Add)
  _              → undef_bb: v2 = Undefined
join_bb:
  result = Phi(uu_bb → v2, ii_bb → v2, ff_bb → v2, undef_bb → v2)
```

When type analysis already knows both args are UInt, guard elimination
collapses the Match to a single arm, the specializer emits `AddUU`, and
the Undefined branch is dead. When types are unknown, the explicit Match
enables the peephole layer to fuse `Match + Add` into type-dispatched
steps without the closure's internal re-dispatch.

**For unary ops** (Not, Neg, BitNot):

```
// Not requires Bool — single-arm Match
Match(v0):
  Bool → not_bb: result = Not(v0)
  _    → undef_bb: result = Undefined
```

**For collection ops** (Len, Index):

```
// Len requires collection type
Match(v0):
  Array → len_bb: result = Len(v0)
  Map   → len_bb: result = Len(v0)
  Text  → len_bb: result = Len(v0)
  Bytes → len_bb: result = Len(v0)
  _     → undef_bb: result = Undefined
```

### Guard + Match by param_type

The pass consults `IntrinsicOp::param_type(index)` for each argument.
The Match arms correspond directly to the types in the `param_type` set
— the same types the Rust `exec_*` function matches on:

| `param_type` constraint | Match arms | Ops |
|-------------------------|------------|-----|
| `uint()` | `UInt` | Bitwise, Shl, Shr, BitTest, BitSet |
| `bool()` | `Bool` | Not |
| `numeric()` | `UInt`, `Int`, `Float` | Add, Sub, Mul, Div, Mod, Lt, Neg |
| `collection()` | `Array`, `Map`, `Text`, `Bytes` | Len, Index |
| `all()` | No guard needed | Eq, MakeArray, MakeMap |

**Convert stays where it is** — the coercion pass inserts `Convert` when
it *knows* both types and can compute the promotion target (e.g. UInt+Int
→ Convert UInt to Int, then Add(Int,Int)). For unknown types, Match is
the honest representation of "we need to check at runtime". Convert is
not suitable for speculative promotion because widening to Float would
lose precision for large integers.

### Domain-specific guards (lowerer or pass)

| Check | Emitted as | Ops | Status |
|-------|-----------|-----|--------|
| `start < end` | `If` | MakeSeq, ArraySeq | Already done |
| `b != 0` | `If` | Div, Mod (integer) | To do |
| `key < len(base)` | `If` | Index (Array/Text/Bytes) | To do |
| `b < 64` | `If` | BitTest, BitSet | To do |

### Interaction with existing passes

- **Coercion insertion** (existing): handles mixed-type arithmetic by
  inserting Convert when both types are known (e.g. UInt+Int → Convert
  UInt to Int, then Add(Int,Int)). The type guard pass handles a different
  concern — ensuring args are numeric at all. These compose: type guards
  ensure numericity, coercion narrows mixed pairs to a common type.

- **Guard elimination** (existing): when type analysis proves the arg
  type, the Match collapses to a single arm, the Undefined branch is
  dead, and CFG simplify removes it. Zero overhead for known types.

- **Definedness analysis** (existing): the `all_defined` flag in the
  compiler already skips `Option` checks. On the success path of a
  Match arm, the arg is proven both defined and typed, enabling
  `all_defined` downstream.

- **Peephole layer** (future): sees `Match(UInt,UInt) → Add` and knows
  the Add closure's internal `match { (UInt,UInt) => ..., _ => None }`
  is redundant. Fuses to `AddUU` — a single closure with no type
  dispatch or definedness check. The Match arms in the IR correspond
  directly to the Rust match arms in the exec closure, making the
  fusion pattern straightforward.

### What NOT to guard

- **MakeArray/MakeMap elements**: dropping undefined is semantic.
- **SeqNext/Collect**: exhaustion is inherently runtime.
- **Convert**: already fully specialized via `try_specialize_convert`.
- **Eq**: accepts `TypeSet::all()` — no constraint to guard.
- **Overflow/NaN**: inherent in checked arithmetic, not separately guardable
  (except div-by-zero, which is guardable).

---

## Summary: Priority for IR Emission

| Priority | Check | Ops affected | Impact |
|----------|-------|-------------|--------|
| **P1** | Bounds check `key < len(base)` | Index | Highest — every `arr[i]` in a loop |
| **P1** | `b != 0` divisor guard | Div, Mod | Enables checked → unchecked div fusion |
| **P1** | Match(type) for constrained args | All with param_type != all() | Enables peephole type fusion |
| **P2** | Guard(defined) for Undefined propagation | MakeSeq, ArraySeq | Small — Undefined propagates naturally |
| **P2** | `b < 64` bit position guard | BitTest, BitSet | Rare — most bit positions are constants |
| -- | MakeArray/MakeMap element guards | MakeArray, MakeMap | Not needed — dropping undefined is semantic |
| -- | Convert type dispatch | Convert | Already complete via specialization |
| -- | SeqNext/Collect exhaustion | SeqNext, Collect | Inherently runtime |
