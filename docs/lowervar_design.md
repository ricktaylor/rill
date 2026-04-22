# LowerVar Design

## Core Principle

**All computation operates on temps.** The lowerer produces and consumes
temporary VarIds for every operation. Named variables are storage —
you write a temp into a slot, you read a slot to get a temp. There is
no direct use of a named variable as an instruction operand.

```
Temps   = values flowing through instructions (VarId)
Slots   = named storage locations (u32 slot ID)
Assign  = write a temp into a slot
Read    = get a temp from a slot
```

## The Type

```rust
/// A value in the lowerer. All values are temps — VarIds defined by
/// exactly one instruction. Named variables are accessed via slots
/// (Assign/Read), which produce temps.
///
/// LowerVar is a transparent wrapper over VarId that prevents the
/// lowerer from accidentally constructing instructions with raw VarIds.
/// All instruction emission goes through helper methods that accept
/// LowerVar and call as_var() internally.
struct LowerVar(VarId);
```

Wait — if everything is a temp, why do we need `LowerVar` at all?
Because the lowerer needs to track **what a value represents** for
binding and narrowing decisions. But the instruction operands are
always VarIds (temps).

The real abstraction the lowerer needs is not a variable type — it's
a clean API where:
- You can't emit an instruction without going through helpers
- Named variable access always goes through read/write
- Narrowing happens automatically at Match boundaries

## Lowerer API

### Value Production (returns VarId)
```rust
fn emit_const(&mut self, value: Literal) -> VarId
fn emit_intrinsic(&mut self, op: IntrinsicOp, args: &[VarId]) -> VarId
fn emit_binary(&mut self, op: IntrinsicOp, a: VarId, b: VarId) -> VarId
fn emit_unary(&mut self, op: IntrinsicOp, a: VarId) -> VarId
fn emit_copy(&mut self, src: VarId) -> VarId
fn emit_index(&mut self, base: VarId, key: VarId) -> VarId
fn emit_call(&mut self, func: FunctionRef, args: Vec<CallArg>) -> VarId
```
Every one returns a fresh VarId (temp). The instruction's `dest` is
created internally. The caller never constructs `Instruction { dest, ... }`
directly.

### Named Variable Access (slot-based)
```rust
/// Create a new named variable slot and write a value into it.
fn bind(&mut self, name: &Identifier, value: VarId)
    // → new_slot(name) + Assign(slot, value)

/// Read a named variable, returning a temp with its current value.
fn read_var(&mut self, name: &Identifier) -> Option<VarId>
    // → lookup_slot(name) + Read(slot) → fresh VarId

/// Write a new value into an existing named variable's slot.
fn reassign(&mut self, name: &Identifier, value: VarId)
    // → lookup_slot(name) + Assign(slot, value)
```

### Type Narrowing (returns VarId with narrowed TypeSet)
```rust
/// Guard: emit Match with Undefined arm. Returns narrowed temp.
fn emit_guard(&mut self, value: VarId, fail_bb: BlockId) -> VarId
    // → Match terminator + Copy with Undefined excluded from TypeSet

/// Match: emit Match with pattern arm. Returns narrowed temp.
fn emit_match(&mut self, value: VarId, pattern: MatchPattern, fail_bb: BlockId) -> VarId
    // → Match terminator + Copy with pattern's TypeSet
```

## Patterns

### let binding
```rill
let x = 1 + 2;
```
```
t0 = emit_const(1)              // Const { dest: t0, value: UInt(1) }
t1 = emit_const(2)              // Const { dest: t1, value: UInt(2) }
t2 = emit_binary(Add, t0, t1)   // Intrinsic { dest: t2, op: Add, args: [t0, t1] }
bind("x", t2)                   // Assign(slot_x, t2)
```

### Variable use in expression
```rill
x + 1
```
```
t0 = read_var("x")              // Read(slot_x) → t0
t1 = emit_const(1)              // Const { dest: t1, value: UInt(1) }
t2 = emit_binary(Add, t0, t1)   // Intrinsic { dest: t2, op: Add, args: [t0, t1] }
```

### Reassignment
```rill
x = x + 1;
```
```
t0 = read_var("x")              // Read(slot_x) → t0
t1 = emit_const(1)              // Const { dest: t1, value: UInt(1) }
t2 = emit_binary(Add, t0, t1)   // Intrinsic { dest: t2, op: Add, args: [t0, t1] }
reassign("x", t2)               // Assign(slot_x, t2)
```

### if let (Variable pattern)
```rill
if let v = maybe_undefined_expr {
    use(v);
}
```
```
t0 = lower_expression(expr)     // some temp
t1 = emit_guard(t0, else_bb)    // Match + narrowing Copy → t1 has Undefined excluded
bind("v", t1)                   // Assign(slot_v, t1)
// body:
t2 = read_var("v")              // Read(slot_v) → t2
emit_call(use, [t2])
```

### if let (Type pattern)
```rill
if let UInt(n) = x {
    return n + 1;
}
```
```
t0 = read_var("x")              // Read(slot_x) → t0
t1 = emit_match(t0, Type(UInt), else_bb)  // Match + narrowing Copy → t1 has {UInt}
bind("n", t1)                   // Assign(slot_n, t1)
// body:
t2 = read_var("n")              // Read(slot_n) → t2
t3 = emit_const(1)
t4 = emit_binary(Add, t2, t3)
// return t4
```

### match expression
```rill
match x {
    UInt(n) => n + 1,
    Text(s) => len(s),
}
```
```
t0 = read_var("x")              // Read(slot_x) → t0

// Multi-arm Match terminator: Match(t0, [(Type(UInt), bb1), (Type(Text), bb2)], default)

// bb1 (UInt arm):
  t1 = Copy(t0) with TypeSet::uint()   // narrowing copy
  bind("n", t1)                         // Assign(slot_n, t1)
  t2 = read_var("n")                    // Read(slot_n) → t2
  t3 = emit_const(1)
  t4 = emit_binary(Add, t2, t3)
  // t4 is the arm result

// bb2 (Text arm):
  t5 = Copy(t0) with TypeSet::text()   // narrowing copy
  bind("s", t5)                         // Assign(slot_s, t5)
  t6 = read_var("s")                    // Read(slot_s) → t6
  t7 = emit_unary(Len, t6)
  // t7 is the arm result

// default (no match):
  t8 = emit_const(Literal::Undefined)

// join:
  result = Phi(bb1 → t4, bb2 → t7, default → t8)
```

### for loop
```rill
for i in 0..10 {
    use(i);
}
```
```
// Setup:
t0 = lower_range(0, 10)         // MakeSeq → Sequence temp

// Header:
t1 = emit_unary(SeqNext, t0)    // next element or Undefined
t2 = emit_guard(t1, exit_bb)    // Match: Undefined → exit, default → body. t2 narrowed.
bind("i", t2)                   // Assign(slot_i, t2)

// Body:
t3 = read_var("i")              // Read(slot_i) → t3
emit_call(use, [t3])
// Jump → header

// Exit:
// loop variable "i" is out of scope
```

### for loop with reassignment
```rill
let sum = 0;
for i in 0..10 {
    sum = sum + i;
}
return sum;
```
```
// Setup:
t0 = emit_const(0)
bind("sum", t0)                  // Assign(slot_sum, t0)
t1 = lower_range(0, 10)

// Header:
t2 = emit_unary(SeqNext, t1)
t3 = emit_guard(t2, exit_bb)
bind("i", t3)

// Body:
t4 = read_var("sum")             // Read(slot_sum) → t4
t5 = read_var("i")               // Read(slot_i) → t5
t6 = emit_binary(Add, t4, t5)
reassign("sum", t6)              // Assign(slot_sum, t6)
// Jump → header

// After mem2reg, slot_sum has a Phi at the header:
//   sum_phi = Phi(setup → t0, body → t6)
// And the Read(slot_sum) in the body resolves to sum_phi.

// Exit:
t7 = read_var("sum")             // Read(slot_sum) → t7, resolves to sum_phi
// return t7
```

### with (reference binding)
```rill
with x = arr[i] {
    x += 1;
}
```
```
t0 = read_var("arr")            // Read(slot_arr) → t0
t1 = read_var("i")              // Read(slot_i) → t1
t2 = emit_make_ref(t0, t1)      // MakeRef { dest: t2, base: t0, key: t1 }
bind_ref("x", t2, origin)       // Assign(slot_x, t2) + record ref origin

// Body:
t3 = read_var("x")              // Read(slot_x) → t3
t4 = emit_const(1)
t5 = emit_binary(Add, t3, t4)
reassign("x", t5)               // Assign(slot_x, t5)
emit_write_ref(t2, t5)          // WriteRef { ref_var: t2, value: t5 }
```

## What mem2reg sees

mem2reg processes the output of the lowerer. It sees:

1. **Assign(slot, temp)** — records the definition of `slot` at this point
2. **Read(slot, dest)** — resolves to the reaching definition's VarId
3. **Everything else** — passes through unchanged (already SSA)

After mem2reg:
- All Assign instructions: removed
- All Read instructions: replaced with Copy (or removed if self-copy)
- Phi nodes: inserted at merge points where a slot has different
  definitions on different paths
- Temp VarIds: unchanged — they were never part of Assign/Read

The result is clean SSA: every VarId defined exactly once, every use
dominated by its definition, one TypeSet per VarId.

## What the lowerer does NOT do

- **Does not construct `Instruction { dest: VarId, ... }` directly.**
  All instruction emission goes through helpers that create the dest
  VarId internally and return it.

- **Does not manage VarId lifetimes.** Temps are ephemeral — created by
  helpers, used as operands, forgotten. The optimizer decides what's dead.

- **Does not insert Phi nodes.** That's mem2reg's job.

- **Does not track types per block.** Each VarId has one TypeSet,
  determined at creation time. Narrowing creates a new VarId with
  a narrower TypeSet — it doesn't modify the original.

- **Does not emit Read/Assign for temps.** Temps are VarIds that go
  directly into instruction operands. Only named variables use slots.

## Migration Strategy

1. Add the new helper methods (`emit_const`, `emit_binary`, etc.)
   alongside existing ones
2. Convert `ir/expr.rs` first — expressions are self-contained
3. Convert `ir/control.rs` — control flow patterns
4. Convert `ir/stmt.rs` — statements
5. Convert `ir/pattern.rs` — pattern matching
6. Remove old direct `self.emit(Instruction { ... })` calls
7. Verify all tests pass at each step
