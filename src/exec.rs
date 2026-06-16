use super::*;
use indexmap::IndexMap;
use std::{
    cell::Cell,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
    rc::Rc,
};
use types::BaseType;

// ============================================================================
// Configuration
// ============================================================================

/// Maximum stack size (catches both value overflow and recursion)
pub const DEFAULT_STACK_SIZE: usize = 65536;

/// Default heap limit (bytes, approximate)
pub const DEFAULT_HEAP_LIMIT: usize = 16 * 1024 * 1024; // 16 MB

/// Maximum Ref indirection depth (guards against circular refs)
const MAX_REF_DEPTH: usize = 64;

// ============================================================================
// Heap tracking
// ============================================================================

/// Shared heap state, held by VM and referenced by all HeapVal instances.
///
/// Uses Cell<usize> for interior mutability of `used` counter.
/// Single-threaded, so no RefCell overhead needed.
#[derive(Debug)]
pub struct Heap {
    used: Cell<usize>,
    limit: usize,
}

impl Heap {
    pub fn new(limit: usize) -> Self {
        Heap {
            used: Cell::new(0),
            limit,
        }
    }

    pub fn used(&self) -> usize {
        self.used.get()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    fn check(&self, size: usize) -> Result<(), ExecError> {
        if self.used.get().saturating_add(size) > self.limit {
            Err(ExecError::HeapOverflow)
        } else {
            Ok(())
        }
    }

    fn alloc(&self, size: usize) {
        self.used.set(self.used.get().saturating_add(size));
    }

    fn dealloc(&self, size: usize) {
        self.used.set(self.used.get().saturating_sub(size));
    }
}

/// Shared reference to heap tracker
pub type HeapRef = Rc<Heap>;

/// Trait for computing heap size of a value
pub trait HeapSize {
    fn heap_size(&self) -> usize;
}

impl HeapSize for Vec<u8> {
    fn heap_size(&self) -> usize {
        self.capacity()
    }
}

impl HeapSize for String {
    fn heap_size(&self) -> usize {
        self.capacity()
    }
}

impl HeapSize for Vec<Value> {
    fn heap_size(&self) -> usize {
        self.capacity() * std::mem::size_of::<Value>()
    }
}

impl HeapSize for SeqState {
    fn heap_size(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl HeapSize for IndexMap<Value, Value> {
    fn heap_size(&self) -> usize {
        self.capacity() * std::mem::size_of::<(Value, Value)>()
    }
}

// ============================================================================
// HeapVal - Heap-allocated value with tracking
// ============================================================================

/// Wrapper that bundles data with its heap tracker.
/// Stored inside Rc so HeapVal itself is just 8 bytes (one pointer).
/// This keeps Value at 16 bytes for better cache locality.
#[derive(Clone)]
struct Tracked<T: Clone> {
    heap: HeapRef,
    data: T,
}

/// Heap-allocated value with automatic tracking.
///
/// - Allocations increment heap counter, deallocations decrement
/// - CoW via make_mut() checks heap limit before cloning
/// - Cheap cloning just bumps Rc refcount
/// - Only 8 bytes (single Rc pointer), keeping Value at 16 bytes
pub struct HeapVal<T: HeapSize + Clone>(Rc<Tracked<T>>);

impl<T: HeapSize + Clone> HeapVal<T> {
    /// Create a new heap-allocated value, tracking the allocation
    pub fn new(data: T, heap: HeapRef) -> Result<Self, ExecError> {
        let size = data.heap_size();
        heap.check(size)?;
        heap.alloc(size);
        Ok(HeapVal(Rc::new(Tracked { heap, data })))
    }

    /// Get current size of this allocation
    pub fn size(&self) -> usize {
        self.0.data.heap_size()
    }

    /// Get mutable access with CoW semantics.
    /// If shared, clones the data (checking heap limit first).
    pub fn make_mut(&mut self, heap: &Heap) -> Result<&mut T, ExecError> {
        if Rc::strong_count(&self.0) > 1 {
            let size = self.size();
            heap.check(size)?;
            heap.alloc(size);
        }
        Ok(&mut Rc::make_mut(&mut self.0).data)
    }

    /// Update heap tracking after mutation. Call after any size-changing operation.
    /// Pass the size() from before the mutation.
    pub fn update_heap_size(&self, old_size: usize, heap: &Heap) -> Result<(), ExecError> {
        let new_size = self.size();
        if new_size > old_size {
            let delta = new_size - old_size;
            heap.check(delta)?;
            heap.alloc(delta);
        } else if new_size < old_size {
            self.0.heap.dealloc(old_size - new_size);
        }
        Ok(())
    }
}

impl<T: HeapSize + Clone> Clone for HeapVal<T> {
    fn clone(&self) -> Self {
        HeapVal(Rc::clone(&self.0))
    }
}

impl<T: HeapSize + Clone> Drop for HeapVal<T> {
    fn drop(&mut self) {
        // If this is the last reference, return allocation to heap
        if Rc::strong_count(&self.0) == 1 {
            self.0.heap.dealloc(self.size());
        }
    }
}

impl<T: HeapSize + Clone> Deref for HeapVal<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.data
    }
}

impl<T: HeapSize + Clone + fmt::Debug> fmt::Debug for HeapVal<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.data.fmt(f)
    }
}

impl<T: HeapSize + Clone + PartialEq> PartialEq for HeapVal<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.data == other.0.data
    }
}

impl<T: HeapSize + Clone + Eq> Eq for HeapVal<T> {}

impl<T: HeapSize + Clone + Hash> Hash for HeapVal<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.data.hash(state);
    }
}

// ============================================================================
// Float wrapper (NaN-free, implements Eq + Hash)
// ============================================================================

/// Float wrapper that implements Eq and Hash.
/// Invariant: Never contains NaN. NaN values become Undefined at runtime.
#[derive(Debug, Clone, Copy, Default)]
pub struct Float(f64);

impl Float {
    pub fn new(f: f64) -> Option<Self> {
        if f.is_nan() { None } else { Some(Float(f)) }
    }

    pub fn get(self) -> f64 {
        self.0
    }

    pub fn new_unchecked(f: f64) -> Self {
        debug_assert!(!f.is_nan(), "Float::new_unchecked called with NaN");
        Float(f)
    }
}

impl Hash for Float {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl PartialEq for Float {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Float {}

// ============================================================================
// Stack Slots
// ============================================================================

/// Frame info stored in a separate frame stack (not in the slot stack).
#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub bp: usize,
    pub return_slot: Option<usize>,
}

/// A slot on the VM stack.
#[derive(Debug, Clone)]
pub enum Slot {
    /// A value (including Value::Undefined for uninitialized slots)
    Val(Value),
    /// Near pointer: reference to another stack slot (absolute index)
    Ref(usize),
    /// Far pointer: accessor into a collection element (base slot + key slot)
    /// Reading resolves to base_collection[key_value].
    /// Writing mutates the collection element at base[key].
    Accessor { base: usize, key: usize },
}

impl Slot {
    pub fn as_value(&self) -> Option<&Value> {
        match self {
            Slot::Val(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_value_mut(&mut self) -> Option<&mut Value> {
        match self {
            Slot::Val(v) => Some(v),
            _ => None,
        }
    }
}

// ============================================================================
// Sequence State
// ============================================================================

/// Internal state for a Sequence value.
///
/// Sequences are single-pass lazy values. Advancing a sequence is a mutation
/// on the shared HeapVal — all references to the same sequence see the
/// same position.
#[derive(Debug, Clone, PartialEq)]
pub enum SeqState {
    /// Range over unsigned integers, always exclusive.
    ///
    /// Inclusive ranges (`..=`) are normalized at construction time by
    /// incrementing end via `saturating_add(1)`. This means `0..=u64::MAX`
    /// saturates to `0..u64::MAX` (missing the last element) — a documented
    /// limitation of the representation.
    Range { current: u64, end: u64 },
    /// Zero-copy slice of an array (e.g., from `..rest` patterns).
    ///
    /// Holds a refcounted reference to the source array — no element copying.
    /// Mutability (by-value vs write-back) is handled at the IR level via
    /// `SliceMode` on the `ArraySeq` intrinsic — the runtime slice doesn't
    /// need to know.
    ArraySlice {
        source: HeapVal<Vec<Value>>,
        start: usize,
        end: usize,
    },
}

impl SeqState {
    /// Advance the sequence and return the next value, or None if exhausted.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Value> {
        match self {
            SeqState::Range { current, end } => {
                if *current >= *end {
                    return None;
                }
                let val = Value::UInt(*current);
                *current += 1;
                Some(val)
            }
            SeqState::ArraySlice { source, start, end } => {
                if *start >= *end {
                    return None;
                }
                let val = source.get(*start).cloned();
                *start += 1;
                val
            }
        }
    }

    /// Remaining length, if known.
    pub fn remaining(&self) -> Option<usize> {
        match self {
            SeqState::Range { current, end } => Some(end.saturating_sub(*current) as usize),
            SeqState::ArraySlice { start, end, .. } => Some(end.saturating_sub(*start)),
        }
    }
}

impl Eq for SeqState {}

impl Hash for SeqState {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            SeqState::Range { current, end } => {
                current.hash(state);
                end.hash(state);
            }
            SeqState::ArraySlice { source, start, end } => {
                // Hash the visible slice contents for value equality
                for val in source.iter().skip(*start).take(*end - *start) {
                    val.hash(state);
                }
            }
        }
    }
}

// ============================================================================
// Values (scalars inline, collections heap-allocated)
// ============================================================================

/// Runtime value. Scalars inline, collections heap-allocated with HeapVal.
///
/// Collections are self-contained (hold Values, not stack indices) so they
/// can safely escape function scope. HeapVal tracks allocations against
/// the VM's heap limit; cloning is cheap (refcount bump); mutation uses
/// CoW semantics via HeapVal::make_mut().
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    // --- Scalar types (inline) ---
    Bool(bool),
    UInt(u64),
    Int(i64),
    Float(Float),

    // --- Heap-allocated types (tracked) ---
    Bytes(HeapVal<Vec<u8>>),
    Text(HeapVal<String>),

    // --- Collection types (heap, self-contained, tracked) ---
    Array(HeapVal<Vec<Value>>),
    /// Map: any value can be a key
    Map(HeapVal<IndexMap<Value, Value>>),

    // --- Sequence type (single-pass lazy values, e.g. ranges) ---
    Sequence(HeapVal<SeqState>),

    // --- Undefined (absence of value) ---
    /// Represents the absence of a value. Produced by failed operations
    /// (overflow, type mismatch, out of bounds) and propagated silently.
    Undefined,
}

impl Value {
    /// Check if this value is defined (not Undefined)
    pub fn is_defined(&self) -> bool {
        !matches!(self, Value::Undefined)
    }

    /// Check if this value is Undefined
    pub fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined)
    }

    pub fn base_type(&self) -> BaseType {
        match self {
            Value::Bool(_) => BaseType::Bool,
            Value::UInt(_) => BaseType::UInt,
            Value::Int(_) => BaseType::Int,
            Value::Float(_) => BaseType::Float,
            Value::Bytes(_) => BaseType::Bytes,
            Value::Text(_) => BaseType::Text,
            Value::Array(_) => BaseType::Array,
            Value::Map(_) => BaseType::Map,
            Value::Sequence(_) => BaseType::Sequence,
            Value::Undefined => BaseType::Undefined,
        }
    }

    pub fn len(&self) -> Option<usize> {
        match self {
            Value::Bytes(b) => Some(b.len()),
            Value::Text(s) => Some(s.chars().count()),
            Value::Array(a) => Some(a.len()),
            Value::Map(m) => Some(m.len()),
            Value::Sequence(it) => it.remaining(),
            _ => None,
        }
    }

    /// Check if the value is empty. Only meaningful for collections and iterators.
    /// Returns false for scalar types (scalars are not "empty").
    pub fn is_empty(&self) -> bool {
        match self.len() {
            Some(n) => n == 0,
            None => false, // scalars are not empty
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash discriminant first for type distinction
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Bool(b) => b.hash(state),
            Value::UInt(n) => n.hash(state),
            Value::Int(n) => n.hash(state),
            Value::Float(f) => f.hash(state),
            Value::Bytes(b) => b.hash(state),
            Value::Text(s) => s.hash(state),
            Value::Array(a) => a.hash(state),
            Value::Map(m) => {
                // Hash map entries in order (IndexMap preserves insertion order)
                m.len().hash(state);
                for (k, v) in m.iter() {
                    k.hash(state);
                    v.hash(state);
                }
            }
            Value::Sequence(it) => it.hash(state),
            Value::Undefined => {} // discriminant already hashed
        }
    }
}

// ============================================================================
// Virtual Machine
// ============================================================================

/// Stack-based virtual machine with heap tracking.
///
/// Frame layout:
/// ```text
/// ┌────────┬────────┬─────┐
/// │ param0 │ local0 │ ... │
/// └────────┴────────┴─────┘
///   bp+0     bp+1
/// ```
///
/// - Frame info (saved BP + return slot) stored on separate frame_stack
/// - IR local offsets start at 0 (params first, then locals)
/// - frame_size = params + locals (no Frame slot overhead)
///
/// Heap tracking:
/// - Collections use HeapVal which tracks allocations via shared Heap
/// - When HeapVal is dropped (refcount → 0), allocation is returned
/// - CoW cloning checks heap limit before allocating
///
/// `clone()` is a deep copy that shares heap accounting (CoW values): use it to
/// derive an independent worker VM that inherits already-initialized globals.
#[derive(Clone)]
pub struct VM {
    stack: Vec<Slot>,
    frame_stack: Vec<FrameInfo>,
    bp: usize,
    heap: HeapRef,
    max_stack_size: usize,
}

impl VM {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_HEAP_LIMIT, DEFAULT_STACK_SIZE)
    }

    pub fn with_heap_limit(heap_limit: usize) -> Self {
        Self::with_limits(heap_limit, DEFAULT_STACK_SIZE)
    }

    pub fn with_limits(heap_limit: usize, max_stack_size: usize) -> Self {
        VM {
            stack: Vec::new(),
            frame_stack: Vec::new(),
            bp: 0,
            heap: Rc::new(Heap::new(heap_limit)),
            max_stack_size,
        }
    }

    /// Reset the VM to a fresh frame and reserve `count` file-scope global
    /// slots (0..count), each initialized to Undefined. Called by `VM::exec`
    /// before running the program's `__init__`.
    pub fn reserve_globals(&mut self, count: usize) {
        self.stack.clear();
        self.frame_stack.clear();
        self.bp = 0;
        self.stack.resize(count, Slot::Val(Value::Undefined));
    }

    // ========================================================================
    // Heap management
    // ========================================================================

    /// Get shared reference to heap (for creating HeapVal instances)
    pub fn heap(&self) -> HeapRef {
        Rc::clone(&self.heap)
    }

    /// Current heap usage
    pub fn heap_used(&self) -> usize {
        self.heap.used()
    }

    /// Heap limit
    pub fn heap_limit(&self) -> usize {
        self.heap.limit()
    }

    /// Current stack pointer (next push location)
    pub fn sp(&self) -> usize {
        self.stack.len()
    }

    /// Current base pointer
    pub fn bp(&self) -> usize {
        self.bp
    }

    // ========================================================================
    // Frame management
    // ========================================================================

    /// Call a function: reserve frame, save BP and return slot in slot 0
    ///
    /// - `frame_size`: number of slots for params + locals
    /// - `return_slot`: absolute index where callee should write return value (None if no return)
    ///
    /// IR local offsets are 0-based (frame info on separate stack).
    pub fn call(&mut self, frame_size: usize, return_slot: Option<usize>) -> Result<(), ExecError> {
        // Stack overflow check
        if self.stack.len() + frame_size > self.max_stack_size {
            return Err(ExecError::StackOverflow);
        }

        let old_bp = self.bp;
        self.bp = self.stack.len();

        // Save frame info on the frame stack (not in the slot stack)
        self.frame_stack.push(FrameInfo {
            bp: old_bp,
            return_slot,
        });

        // Reserve entire frame at once — all slots start as Undefined
        self.stack
            .resize(self.bp + frame_size, Slot::Val(Value::Undefined));

        Ok(())
    }

    /// Call a function, adopting the top `argc` values on the stack as arguments.
    ///
    /// The embedder pushes args before calling this method (Lua-style):
    /// ```ignore
    /// vm.push(Value::UInt(42))?;
    /// vm.push(Value::Text("hello".into()))?;
    /// vm.call_with_args(frame_size, 2)?;
    /// // Args are now in slots 0..argc of the new frame
    /// ```
    ///
    /// Internally, the pushed values are shifted right by one slot to make
    /// room for the Frame info at slot 0. On `ret()`, the entire frame
    /// (including adopted args) is cleaned up.
    pub fn call_with_args(&mut self, frame_size: usize, argc: usize) -> Result<(), ExecError> {
        if frame_size < argc {
            return Err(ExecError::StackOverflow);
        }

        let args_base = self.stack.len() - argc;

        // Stack overflow check
        if args_base + frame_size > self.max_stack_size {
            return Err(ExecError::StackOverflow);
        }

        let old_bp = self.bp;
        self.bp = args_base;

        // Save frame info on the frame stack
        self.frame_stack.push(FrameInfo {
            bp: old_bp,
            return_slot: None,
        });

        // Extend to full frame size — args are already in place at bp,
        // no rotate needed (Frame slot no longer occupies slot 0).
        self.stack
            .resize(self.bp + frame_size, Slot::Val(Value::Undefined));

        Ok(())
    }

    /// Return from function: restore BP, truncate stack
    pub fn ret(&mut self) {
        let saved_bp = self.frame_stack.pop().map(|info| info.bp).unwrap_or(0);

        self.stack.truncate(self.bp);
        self.bp = saved_bp;
    }

    /// Return from function with a value: write to caller's return slot, then cleanup
    ///
    /// Writes directly to the return_slot specified in call(), avoiding copies.
    pub fn ret_val(&mut self, value: Value) {
        let info = self.frame_stack.pop().unwrap_or(FrameInfo {
            bp: 0,
            return_slot: None,
        });

        // Write return value directly to caller's slot (must happen before truncate)
        if let Some(slot) = info.return_slot
            && let Some(s) = self.stack.get_mut(slot)
        {
            *s = Slot::Val(value);
        }

        self.stack.truncate(self.bp);
        self.bp = info.bp;
    }

    /// Bind a parameter after call() setup
    ///
    /// - `offset`: parameter slot (1-based, 0 is Frame info)
    /// - `arg_idx`: absolute stack index of the argument value
    /// - `by_ref`: if true, creates Ref (mutations flow back); if false, copies value
    pub fn bind_param(&mut self, offset: usize, arg_idx: usize, by_ref: bool) {
        let slot = self.bp + offset;
        if slot >= self.stack.len() {
            return;
        }
        if by_ref {
            self.stack[slot] = Slot::Ref(arg_idx);
        } else if let Some(val) = self.get(arg_idx) {
            self.stack[slot] = Slot::Val(val.clone());
        }
    }

    // ========================================================================
    // Slot access
    // ========================================================================

    /// Resolve a slot index, following Ref indirection.
    /// Uses an iteration limit to prevent infinite loops on circular refs.
    pub fn resolve(&self, idx: usize) -> usize {
        let mut current = idx;
        // Ref chains should be short (typically 1 hop). A limit prevents
        // infinite loops if circular refs are ever constructed by a bug.
        for _ in 0..MAX_REF_DEPTH {
            match self.stack.get(current) {
                Some(Slot::Ref(target)) => current = *target,
                _ => return current,
            }
        }
        current
    }

    /// Read a value from a slot, following Refs and reading through Accessors.
    ///
    /// - Val: returns the value directly
    /// - Ref: follows the chain recursively
    /// - Accessor: reads base_collection[key_value] from the collection
    pub fn get(&self, idx: usize) -> Option<&Value> {
        match self.stack.get(idx)? {
            Slot::Val(v) => Some(v),
            Slot::Ref(target) => self.get(*target),
            Slot::Accessor { base, key } => {
                let base_val = self.get(*base)?;
                let key_val = self.get(*key)?;
                match (base_val, key_val) {
                    (Value::Array(arr), Value::UInt(i)) => arr.get(*i as usize),
                    (Value::Array(arr), Value::Int(i)) if *i >= 0 => arr.get(*i as usize),
                    (Value::Map(map), k) => map.get(k),
                    _ => None,
                }
            }
        }
    }

    /// Get value at local offset (bp + offset), resolving Refs and Accessors.
    pub fn local(&self, offset: usize) -> &Value {
        self.get(self.bp + offset)
            .expect("local: could not read slot")
    }

    /// Write a value to a slot, following Refs and writing through Accessors.
    ///
    /// - Val: replaces the value
    /// - Ref: follows the chain recursively
    /// - Accessor: mutates the collection element at base[key]
    pub fn set(&mut self, idx: usize, value: Value) {
        let (base, key) = match &self.stack[idx] {
            Slot::Val(_) => {
                self.stack[idx] = Slot::Val(value);
                return;
            }
            Slot::Ref(target) => {
                let t = *target;
                return self.set(t, value);
            }
            Slot::Accessor { base, key } => (*base, *key),
        };
        // Write through Accessor: mutate the collection element
        let key_val = self.get(key).cloned().unwrap_or(Value::Undefined);
        match &key_val {
            Value::UInt(i) => {
                let _ = self.set_array_elem(base, *i as usize, value);
            }
            _ => {
                let _ = self.set_map_entry(base, key_val, value);
            }
        }
    }

    /// Set value at local offset (resolves refs — writes through to target)
    pub fn set_local(&mut self, offset: usize, value: Value) {
        self.set(self.bp + offset, value);
    }

    /// Reset a local slot to Val(Undefined), regardless of its current type
    /// (Val, Ref, or Accessor). Used by TailCall to simulate a fresh frame.
    /// Unlike set_local, this does NOT resolve through Refs or Accessors.
    pub fn reset_local(&mut self, offset: usize) {
        self.stack[self.bp + offset] = Slot::Val(Value::Undefined);
    }

    /// Create a Slot::Accessor (far pointer) at a local offset.
    /// The accessor indexes into a collection at base_abs[key_abs].
    pub fn set_accessor(&mut self, offset: usize, base_abs: usize, key_abs: usize) {
        let idx = self.bp + offset;
        if idx < self.stack.len() {
            self.stack[idx] = Slot::Accessor {
                base: base_abs,
                key: key_abs,
            };
        }
    }

    /// Shallow-copy a slot from an absolute source address to a local destination.
    /// Preserves Slot::Ref/Accessor identity — copies the slot as-is.
    /// Used for uniform call arg setup (ref-agnostic).
    pub fn copy_slot_from(&mut self, dst_offset: usize, src_abs: usize) {
        let dst = self.bp + dst_offset;
        if src_abs < self.stack.len() && dst < self.stack.len() {
            self.stack[dst] = self.stack[src_abs].clone();
        }
    }

    // ========================================================================
    // Extern argument access (Lua-style stack API)
    // ========================================================================

    /// Get function argument by 0-based index.
    ///
    /// Args occupy slots 1..=N in the current frame (slot 0 is Frame info).
    /// `arg(0)` returns the first argument, `arg(1)` the second, etc.
    ///
    /// ```ignore
    /// fn my_extern(vm: &mut VM, argc: usize) -> Result<ExecResult, ExecError> {
    ///     let x = vm.arg(0).clone();
    ///     let y = vm.arg(1).clone();
    ///     Ok(ExecResult::Return(Value::UInt(x + y)))
    /// }
    /// ```
    pub fn arg(&self, index: usize) -> &Value {
        self.local(index) // args start at slot 0 (no Frame slot)
    }

    /// Set a slot to be a reference to another slot
    pub fn set_ref(&mut self, idx: usize, target: usize) {
        if let Some(slot) = self.stack.get_mut(idx) {
            *slot = Slot::Ref(target);
        }
    }

    /// Set local slot to be a reference
    pub fn set_local_ref(&mut self, offset: usize, target: usize) {
        self.set_ref(self.bp + offset, target);
    }

    // ========================================================================
    // Collection operations (values are self-contained, use CoW via HeapVal::make_mut)
    // ========================================================================

    /// Index into array: returns cloned element value
    pub fn index_array(&self, arr_idx: usize, elem_idx: usize) -> Option<Value> {
        match self.get(arr_idx)? {
            Value::Array(arr) => arr.get(elem_idx).cloned(),
            _ => None,
        }
    }

    /// Index into map: returns cloned value
    pub fn index_map(&self, map_idx: usize, key: &Value) -> Option<Value> {
        match self.get(map_idx)? {
            Value::Map(map) => map.get(key).cloned(),
            _ => None,
        }
    }

    /// Set element in array (CoW: clones if shared, checks heap limit)
    pub fn set_array_elem(
        &mut self,
        arr_idx: usize,
        elem_idx: usize,
        value: Value,
    ) -> Result<bool, ExecError> {
        let resolved = self.resolve(arr_idx);
        // Get heap ref before mutable borrow of stack
        let heap = &*self.heap;
        match self.stack.get_mut(resolved).and_then(|s| s.as_value_mut()) {
            Some(Value::Array(arr)) if elem_idx < arr.len() => {
                arr.make_mut(heap)?[elem_idx] = value;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Set or insert entry in map (CoW: clones if shared, checks heap limit)
    pub fn set_map_entry(
        &mut self,
        map_idx: usize,
        key: Value,
        value: Value,
    ) -> Result<bool, ExecError> {
        let resolved = self.resolve(map_idx);
        // Get heap ref before mutable borrow of stack
        let heap = &*self.heap;
        match self.stack.get_mut(resolved).and_then(|s| s.as_value_mut()) {
            Some(Value::Map(map)) => {
                map.make_mut(heap)?.insert(key, value);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Append a value to an array (CoW: clones if shared, checks heap limit).
    ///
    /// Returns `Ok(true)` if the append succeeded, `Ok(false)` if the slot
    /// is not an array.
    pub fn array_append(&mut self, arr_idx: usize, value: Value) -> Result<bool, ExecError> {
        let resolved = self.resolve(arr_idx);
        let heap = &*self.heap;
        match self.stack.get_mut(resolved).and_then(|s| s.as_value_mut()) {
            Some(Value::Array(arr)) => {
                let old_size = arr.size();
                arr.make_mut(heap)?.push(value);
                arr.update_heap_size(old_size, heap)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    // ========================================================================
    // Value construction (pushes to stack, returns index)
    // ========================================================================

    /// Push a value, return its absolute index
    pub fn push(&mut self, value: Value) -> Result<usize, ExecError> {
        if self.stack.len() >= self.max_stack_size {
            return Err(ExecError::StackOverflow);
        }
        let idx = self.stack.len();
        self.stack.push(Slot::Val(value));
        Ok(idx)
    }

    /// Advance a Sequence at the given absolute index, returning the next value.
    /// Returns None if the slot is not a Sequence or the sequence is exhausted.
    /// Mutates the sequence in-place (CoW: clones if shared).
    pub fn seq_next(&mut self, idx: usize) -> Result<Option<Value>, ExecError> {
        let resolved = self.resolve(idx);
        let heap = &*self.heap;
        match self.stack.get_mut(resolved).and_then(|s| s.as_value_mut()) {
            Some(Value::Sequence(seq)) => {
                let state = seq.make_mut(heap)?;
                Ok(state.next())
            }
            _ => Ok(None),
        }
    }

    /// Drain all remaining elements from a Sequence, returning them as an Array.
    /// Returns None if the slot is not a Sequence.
    /// Mutates the sequence in-place (CoW: clones if shared).
    pub fn seq_collect(&mut self, idx: usize) -> Result<Option<Value>, ExecError> {
        let resolved = self.resolve(idx);
        let heap = &*self.heap;
        match self.stack.get_mut(resolved).and_then(|s| s.as_value_mut()) {
            Some(Value::Sequence(seq)) => {
                let state = seq.make_mut(heap)?;
                let mut elements = Vec::new();
                while let Some(val) = state.next() {
                    elements.push(val);
                }
                let arr = HeapVal::new(elements, self.heap.clone())?;
                Ok(Some(Value::Array(arr)))
            }
            _ => Ok(None),
        }
    }

    pub fn push_bool(&mut self, b: bool) -> Result<usize, ExecError> {
        self.push(Value::Bool(b))
    }

    pub fn push_uint(&mut self, n: u64) -> Result<usize, ExecError> {
        self.push(Value::UInt(n))
    }

    pub fn push_int(&mut self, n: i64) -> Result<usize, ExecError> {
        self.push(Value::Int(n))
    }

    pub fn push_float(&mut self, f: f64) -> Result<Option<usize>, ExecError> {
        match Float::new(f) {
            Some(f) => self.push(Value::Float(f)).map(Some),
            None => Ok(None), // NaN → Undefined
        }
    }

    pub fn push_text(&mut self, s: impl Into<String>) -> Result<usize, ExecError> {
        let heap = self.heap();
        self.push(Value::Text(HeapVal::new(s.into(), heap)?))
    }

    pub fn push_bytes(&mut self, b: impl Into<Vec<u8>>) -> Result<usize, ExecError> {
        let heap = self.heap();
        self.push(Value::Bytes(HeapVal::new(b.into(), heap)?))
    }

    pub fn push_array(&mut self, elements: Vec<Value>) -> Result<usize, ExecError> {
        let heap = self.heap();
        self.push(Value::Array(HeapVal::new(elements, heap)?))
    }

    pub fn push_map(&mut self, entries: IndexMap<Value, Value>) -> Result<usize, ExecError> {
        let heap = self.heap();
        self.push(Value::Map(HeapVal::new(entries, heap)?))
    }

    pub fn push_empty_map(&mut self) -> Result<usize, ExecError> {
        let heap = self.heap();
        self.push(Value::Map(HeapVal::new(IndexMap::new(), heap)?))
    }

    pub fn push_empty_array(&mut self) -> Result<usize, ExecError> {
        let heap = self.heap();
        self.push(Value::Array(HeapVal::new(Vec::new(), heap)?))
    }
}

impl Default for VM {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Execution types
// ============================================================================

/// Execution error
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// Stack overflow (value or recursion)
    #[error("stack overflow")]
    StackOverflow,
    /// Heap allocation limit exceeded
    #[error("heap allocation limit exceeded")]
    HeapOverflow,
}
