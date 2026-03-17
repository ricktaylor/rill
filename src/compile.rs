//! IR-to-Closure Compilation and Execution
//!
//! Compiles the SSA-form IR into closure-threaded code for execution.
//! Each IR instruction becomes a Rust closure that captures its operands
//! (slot offsets, resolved builtin function pointers, constant values).
//!
//! # Architecture
//!
//! ```text
//! IrProgram → compile_program() → CompiledProgram
//!                                      │
//!                                      ▼
//!                               execute() loop
//!                               ┌──────────────┐
//!                               │ select block  │ ← outer loop (iterative)
//!                               │ run steps     │ ← sequential closures
//!                               │ terminator    │ ← selects next block
//!                               └──────────────┘
//! ```
//!
//! # Design Notes
//!
//! - No per-instruction dispatch switch — closures ARE the instructions
//! - Builtins resolved once at compile time (no runtime HashMap lookup)
//! - VarIds mapped to stack slot offsets at compile time
//! - Loops use iterative block dispatch (no Rust stack growth)
//! - User function calls use recursive `execute_function` (bounded by VM stack limit)
//! - Future: tail-call optimization can convert recursive calls to jumps

use crate::builtins::{BuiltinImpl, BuiltinRegistry, ExecResult};
use crate::diagnostics::Diagnostics;
use crate::exec::{ExecError, Float, HeapVal, VM, Value};
use crate::ir::{
    BasicBlock, BlockId, Function, Instruction, IntrinsicOp, IrProgram, Literal, MatchPattern,
    Terminator, VarId,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

// ============================================================================
// Compiled Types
// ============================================================================

/// A compiled program ready for execution.
pub struct CompiledProgram {
    functions: Vec<CompiledFunction>,
    /// Function name → index into `functions`
    pub(crate) func_index: HashMap<String, usize>,
    /// Warnings from compilation (unused functions, etc.)
    pub warnings: Diagnostics,
}

/// A compiled function — flat array of step closures with block offsets.
struct CompiledFunction {
    /// All step closures for all blocks, flattened into a single contiguous array.
    /// Block boundaries are recorded in `block_starts`.
    steps: Vec<Step>,
    /// Offset into `steps` where each block begins.
    /// `block_starts[i]` is the index of the first step in block i.
    block_starts: Vec<usize>,
    entry: usize, // index into block_starts
    frame_size: usize,
    param_count: usize,
}

/// A step closure. Captures operands, operates on VM.
/// Instructions return Continue; terminators return NextBlock/Return/Exit.
type Step = Box<dyn Fn(&mut VM, &CompiledProgram) -> Result<Action, ExecError>>;

/// Result of executing a step.
enum Action {
    /// Continue to the next step in this block
    Continue,
    /// Jump to another block
    NextBlock(usize),
    /// Return from function
    Return(Option<Value>),
    /// Hard exit to driver (diverging builtin)
    Exit(Value),
}

// ============================================================================
// Slot Mapping
// ============================================================================

/// Maps VarIds to stack slot offsets.
/// Slot 0 = Frame info. VarId(n) → slot n + 1.
fn slot(var: VarId) -> usize {
    var.0 as usize + 1
}

/// Maps BlockId to index in the compiled blocks array.
fn build_block_map(blocks: &[BasicBlock]) -> HashMap<BlockId, usize> {
    blocks
        .iter()
        .enumerate()
        .map(|(idx, b)| (b.id, idx))
        .collect()
}

// ============================================================================
// Literal → Value Conversion
// ============================================================================

fn literal_to_value(lit: &Literal, heap: &crate::exec::HeapRef) -> Result<Value, ExecError> {
    Ok(match lit {
        Literal::Bool(b) => Value::Bool(*b),
        Literal::UInt(n) => Value::UInt(*n),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => {
            match crate::exec::Float::new(*f) {
                Some(float) => Value::Float(float),
                None => return Ok(Value::Bool(false)), // NaN → treat as undefined
            }
        }
        Literal::Text(s) => Value::Text(HeapVal::new(s.clone(), heap.clone())?),
        Literal::Bytes(b) => Value::Bytes(HeapVal::new(b.clone(), heap.clone())?),
    })
}

// ============================================================================
// Match Pattern Testing
// ============================================================================

fn test_match_pattern(pattern: &MatchPattern, value: &Value) -> bool {
    match pattern {
        MatchPattern::Literal(lit) => match (lit, value) {
            (Literal::Bool(a), Value::Bool(b)) => a == b,
            (Literal::UInt(a), Value::UInt(b)) => a == b,
            (Literal::Int(a), Value::Int(b)) => a == b,
            (Literal::Float(a), Value::Float(b)) => crate::exec::Float::new(*a)
                .map(|f| f == *b)
                .unwrap_or(false),
            (Literal::Text(a), Value::Text(b)) => a.as_str() == **b,
            (Literal::Bytes(a), Value::Bytes(b)) => a.as_slice() == **b,
            _ => false,
        },
        MatchPattern::Type(base_type) => value.base_type() == *base_type,
        MatchPattern::Array(len) => matches!(value, Value::Array(a) if a.len() == *len),
        MatchPattern::ArrayMin(min) => matches!(value, Value::Array(a) if a.len() >= *min),
    }
}

// ============================================================================
// Compilation: IR → Closures
// ============================================================================

/// Compile an IR program into closure-threaded code.
///
/// Includes a link phase that resolves all function references at compile time
/// and emits diagnostics for undefined or unused functions.
pub fn compile_program(
    ir: &IrProgram,
    builtins: &BuiltinRegistry,
) -> Result<CompiledProgram, Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    // Build user function index
    let mut func_index: HashMap<String, usize> = HashMap::new();
    for (idx, ir_func) in ir.functions.iter().enumerate() {
        func_index.insert(ir_func.name.to_string(), idx);
    }

    // Link phase: resolve all Call references, track usage
    let mut used_functions: HashSet<usize> = HashSet::new();
    let link_map = link_functions(
        ir,
        builtins,
        &func_index,
        &mut used_functions,
        &mut diagnostics,
    );

    if diagnostics.has_errors() {
        return Err(diagnostics);
    }

    // TODO: Warn about unused functions once pub/priv distinction exists.
    // Currently all functions are potential entry points (callable by embedder),
    // so we can't know which are truly unused without #[export] or pub/priv.
    let _ = used_functions;

    // Compile functions to closures
    let mut compiled_functions = Vec::new();
    for ir_func in &ir.functions {
        match compile_function(ir_func, &link_map) {
            Ok(compiled) => compiled_functions.push(compiled),
            Err(_) => {
                diagnostics.error_no_span(
                    crate::diagnostics::DiagnosticCode::E500_UndefinedExternal,
                    format!("internal error compiling function `{}`", ir_func.name),
                );
                return Err(diagnostics);
            }
        }
    }

    let mut program = CompiledProgram {
        functions: compiled_functions,
        func_index,
        warnings: Diagnostics::new(),
    };

    // Attach warnings to the program for the caller
    program.warnings = diagnostics;

    Ok(program)
}

/// Resolution of a function call — determined at link time.
#[derive(Clone)]
enum CallTarget {
    /// Native builtin — function pointer resolved at compile time
    Builtin(crate::builtins::BuiltinFn),
    /// User-defined function — index into CompiledProgram.functions
    UserFunction(usize),
}

/// Map from qualified function name to its resolved target.
type LinkMap = HashMap<String, CallTarget>;

/// Link phase: resolve all function references and track usage.
fn link_functions(
    ir: &IrProgram,
    builtins: &BuiltinRegistry,
    func_index: &HashMap<String, usize>,
    used_functions: &mut HashSet<usize>,
    diagnostics: &mut Diagnostics,
) -> LinkMap {
    let mut link_map = LinkMap::new();

    // Pre-populate with all builtins
    for (name, def) in builtins.iter() {
        if let BuiltinImpl::Native(f) = &def.implementation {
            link_map.insert(name.clone(), CallTarget::Builtin(*f));
        }
    }

    // Pre-populate with all user functions
    for (name, &idx) in func_index {
        link_map.insert(name.clone(), CallTarget::UserFunction(idx));
    }

    // Walk all Call instructions and verify references resolve
    for ir_func in &ir.functions {
        for block in &ir_func.blocks {
            for inst in &block.instructions {
                if let Instruction::Call { function, .. } = &inst.node {
                    let qname = function.qualified_name();
                    if let Some(target) = link_map.get(&qname) {
                        // Track user function usage
                        if let CallTarget::UserFunction(idx) = target {
                            used_functions.insert(*idx);
                        }
                    } else {
                        diagnostics.error(
                            crate::diagnostics::DiagnosticCode::E500_UndefinedExternal,
                            inst.span,
                            format!("undefined function `{}`", qname),
                        );
                    }
                }
            }
        }
    }

    link_map
}

fn compile_function(func: &Function, link_map: &LinkMap) -> Result<CompiledFunction, ExecError> {
    let block_map = build_block_map(&func.blocks);
    let frame_size = 1 + func.locals.len(); // slot 0 = Frame, then locals

    // First pass: compile all blocks, collecting phi metadata
    let mut blocks = Vec::new();
    let mut pending_phis: Vec<(usize, usize, Vec<(usize, usize)>)> = Vec::new();
    // pending_phis: (dest_slot, join_block_idx, [(pred_block_idx, src_slot)])

    for ir_block in &func.blocks {
        let join_idx = block_map[&ir_block.id];

        // Collect phis from this block
        for inst in &ir_block.instructions {
            if let Instruction::Phi { dest, sources } = &inst.node {
                let d = slot(*dest);
                let compiled_sources: Vec<(usize, usize)> = sources
                    .iter()
                    .filter_map(|(block_id, var_id)| {
                        block_map.get(block_id).map(|&idx| (idx, slot(*var_id)))
                    })
                    .collect();

                // Skip identity phis (all sources are the dest slot)
                let all_same_as_dest =
                    !compiled_sources.is_empty() && compiled_sources.iter().all(|(_, s)| *s == d);
                if !compiled_sources.is_empty() && !all_same_as_dest {
                    pending_phis.push((d, join_idx, compiled_sources));
                }
            }
        }

        blocks.push(compile_block(ir_block, &block_map, link_map)?);
    }

    // Second pass: resolve phis by inserting copies into predecessor blocks.
    // Each phi source (pred_block, src_slot) → insert Copy(dest, src) before
    // the terminator (last element) of the predecessor block.
    for (dest_slot, _join_idx, sources) in pending_phis {
        for (pred_block_idx, src_slot) in sources {
            if src_slot != dest_slot {
                let d = dest_slot;
                let s = src_slot;
                let block = &mut blocks[pred_block_idx];
                let insert_pos = if block.is_empty() { 0 } else { block.len() - 1 };
                block.insert(
                    insert_pos,
                    Box::new(move |vm: &mut VM, _prog| {
                        if let Some(val) = vm.local(s).cloned() {
                            vm.set_local(d, val);
                        } else {
                            vm.set_local_uninit(d);
                        }
                        Ok(Action::Continue)
                    }),
                );
            }
        }
    }

    // Flatten blocks into a single contiguous step array with offsets.
    let mut steps: Vec<Step> = Vec::new();
    let mut block_starts: Vec<usize> = Vec::new();
    for block in blocks {
        block_starts.push(steps.len());
        steps.extend(block);
    }

    let entry = *block_map
        .get(&func.entry_block)
        .expect("entry block must exist");

    Ok(CompiledFunction {
        steps,
        block_starts,
        entry,
        frame_size,
        param_count: func.params.len(),
    })
}

fn compile_block(
    block: &BasicBlock,
    block_map: &HashMap<BlockId, usize>,
    link_map: &LinkMap,
) -> Result<Vec<Step>, ExecError> {
    let mut steps: Vec<Step> = Vec::new();

    for spanned_inst in &block.instructions {
        match &spanned_inst.node {
            // Phis are handled in compile_function's second pass —
            // copies are inserted into predecessor blocks
            Instruction::Phi { .. } => {}
            inst => {
                if let Some(step) = compile_instruction(inst, block_map, link_map)? {
                    steps.push(step);
                }
            }
        }
    }

    // Terminator is the last step in the block
    steps.push(compile_terminator(&block.terminator, block_map)?);

    Ok(steps)
}

fn compile_instruction(
    inst: &Instruction,
    _block_map: &HashMap<BlockId, usize>,
    link_map: &LinkMap,
) -> Result<Option<Step>, ExecError> {
    Ok(Some(match inst {
        Instruction::Const { dest, value } => {
            let d = slot(*dest);
            let lit = value.clone();
            Box::new(move |vm: &mut VM, _prog| {
                let val = literal_to_value(&lit, &vm.heap())?;
                vm.set_local(d, val);
                Ok(Action::Continue)
            })
        }

        Instruction::Copy { dest, src } => {
            let d = slot(*dest);
            let s = slot(*src);
            Box::new(move |vm: &mut VM, _prog| {
                if let Some(val) = vm.local(s).cloned() {
                    vm.set_local(d, val);
                }
                Ok(Action::Continue)
            })
        }

        Instruction::Undefined { dest } => {
            let d = slot(*dest);
            Box::new(move |vm: &mut VM, _prog| {
                vm.set_local_uninit(d);
                Ok(Action::Continue)
            })
        }

        Instruction::Index { dest, base, key } => {
            let d = slot(*dest);
            let b = slot(*base);
            let k = slot(*key);
            Box::new(move |vm: &mut VM, _prog| {
                let result = match (vm.local(b), vm.local(k)) {
                    (Some(base_val), Some(key_val)) => index_value(base_val, key_val),
                    _ => None,
                };
                match result {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        }

        Instruction::SetIndex { base, key, value } => {
            let b = slot(*base);
            let k = slot(*key);
            let v = slot(*value);
            Box::new(move |vm: &mut VM, _prog| {
                if let (Some(key_val), Some(new_val)) = (vm.local(k).cloned(), vm.local(v).cloned())
                {
                    // Use the VM's collection mutation methods
                    match &key_val {
                        Value::UInt(idx) => {
                            let _ = vm.set_array_elem(vm.bp() + b, *idx as usize, new_val);
                        }
                        _ => {
                            let _ = vm.set_map_entry(vm.bp() + b, key_val, new_val);
                        }
                    }
                }
                Ok(Action::Continue)
            })
        }

        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let d = slot(*dest);
            let arg_slots: Vec<(usize, bool)> =
                args.iter().map(|a| (slot(a.value), a.by_ref)).collect();
            let func_name = function.qualified_name();

            // Resolve via link map (all references verified at link time)
            match link_map.get(&func_name).cloned() {
                Some(CallTarget::Builtin(f)) => Box::new(move |vm: &mut VM, _prog| {
                    let arg_values: Vec<Value> = arg_slots
                        .iter()
                        .filter_map(|(s, _)| vm.local(*s).cloned())
                        .collect();
                    match f(vm, &arg_values)? {
                        ExecResult::Return(Some(val)) => vm.set_local(d, val),
                        ExecResult::Return(None) => vm.set_local_uninit(d),
                        ExecResult::Exit(val) => return Ok(Action::Exit(val)),
                    }
                    Ok(Action::Continue)
                }),
                Some(CallTarget::UserFunction(func_idx)) => {
                    Box::new(move |vm: &mut VM, prog: &CompiledProgram| {
                        let arg_values: Vec<(Value, bool)> = arg_slots
                            .iter()
                            .filter_map(|(s, by_ref)| vm.local(*s).cloned().map(|v| (v, *by_ref)))
                            .collect();
                        match execute_function(prog, vm, func_idx, &arg_values)? {
                            Action::Return(Some(val)) => vm.set_local(d, val),
                            Action::Return(None) => vm.set_local_uninit(d),
                            Action::Exit(val) => return Ok(Action::Exit(val)),
                            Action::Continue | Action::NextBlock(_) => unreachable!(),
                        }
                        Ok(Action::Continue)
                    })
                }
                None => {
                    // Unresolved — link phase should have caught this.
                    // Emit undefined as fallback.
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local_uninit(d);
                        Ok(Action::Continue)
                    })
                }
            }
        }

        Instruction::Intrinsic { dest, op, args } => {
            let d = slot(*dest);
            let op = *op;
            let arg_slots: Vec<usize> = args.iter().map(|v| slot(*v)).collect();
            Box::new(move |vm: &mut VM, _prog| {
                let result = exec_intrinsic(op, &arg_slots, vm)?;
                match result {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        }

        Instruction::MakeRef { dest, base, key } => {
            // TODO: proper reference tracking for write-back
            // For now, treat as Index (read the value)
            let d = slot(*dest);
            let b = slot(*base);
            let k = slot(*key);
            Box::new(move |vm: &mut VM, _prog| {
                let result = match (vm.local(b), vm.local(k)) {
                    (Some(base_val), Some(key_val)) => index_value(base_val, key_val),
                    _ => None,
                };
                match result {
                    Some(val) => vm.set_local(d, val),
                    None => vm.set_local_uninit(d),
                }
                Ok(Action::Continue)
            })
        }

        Instruction::Drop { .. } => {
            // No-op for now — slots are reclaimed when frame is popped
            return Ok(None);
        }

        // Phi is handled separately in compile_block
        Instruction::Phi { .. } => return Ok(None),
    }))
}

fn compile_terminator(
    term: &Terminator,
    block_map: &HashMap<BlockId, usize>,
) -> Result<Step, ExecError> {
    Ok(match term {
        Terminator::Jump { target } => {
            let idx = block_map[target];
            Box::new(move |_vm, _prog| Ok(Action::NextBlock(idx)))
        }

        Terminator::If {
            condition,
            then_target,
            else_target,
            ..
        } => {
            let cond_slot = slot(*condition);
            let then_idx = block_map[then_target];
            let else_idx = block_map[else_target];
            Box::new(move |vm: &mut VM, _prog| {
                let is_true = vm
                    .local(cond_slot)
                    .map(|v| matches!(v, Value::Bool(true)))
                    .unwrap_or(false);
                Ok(Action::NextBlock(if is_true { then_idx } else { else_idx }))
            })
        }

        Terminator::Match {
            value,
            arms,
            default,
            ..
        } => {
            let val_slot = slot(*value);
            let compiled_arms: Vec<(MatchPattern, usize)> = arms
                .iter()
                .map(|(pat, target)| (pat.clone(), block_map[target]))
                .collect();
            let default_idx = block_map[default];
            Box::new(move |vm: &mut VM, _prog| {
                if let Some(val) = vm.local(val_slot) {
                    for (pattern, target_idx) in &compiled_arms {
                        if test_match_pattern(pattern, val) {
                            return Ok(Action::NextBlock(*target_idx));
                        }
                    }
                }
                Ok(Action::NextBlock(default_idx))
            })
        }

        Terminator::Guard {
            value,
            defined,
            undefined,
            ..
        } => {
            let val_slot = slot(*value);
            let def_idx = block_map[defined];
            let undef_idx = block_map[undefined];
            Box::new(move |vm: &mut VM, _prog| {
                let is_defined = vm.local(val_slot).is_some();
                Ok(Action::NextBlock(if is_defined {
                    def_idx
                } else {
                    undef_idx
                }))
            })
        }

        Terminator::Return { value } => {
            let val_slot = value.map(slot);
            Box::new(move |vm: &mut VM, _prog| {
                let val = val_slot.and_then(|s| vm.local(s).cloned());
                Ok(Action::Return(val))
            })
        }

        Terminator::Exit { value } => {
            let val_slot = slot(*value);
            Box::new(move |vm: &mut VM, _prog| {
                let val = vm.local(val_slot).cloned().unwrap_or(Value::UInt(0));
                Ok(Action::Exit(val))
            })
        }

        Terminator::Unreachable => Box::new(|_vm, _prog| Ok(Action::Return(None))),
    })
}

// ============================================================================
// Value Indexing (runtime)
// ============================================================================

fn index_value(base: &Value, key: &Value) -> Option<Value> {
    match (base, key) {
        (Value::Array(arr), Value::UInt(idx)) => arr.get(*idx as usize).cloned(),
        (Value::Array(arr), Value::Int(idx)) if *idx >= 0 => arr.get(*idx as usize).cloned(),
        (Value::Map(map), key) => map.get(key).cloned(),
        (Value::Text(s), Value::UInt(idx)) => {
            s.chars().nth(*idx as usize).map(|c| {
                // Text indexing returns a single-char string — but we need HeapVal.
                // For now, return UInt of the char code. TODO: return Text properly.
                Value::UInt(c as u64)
            })
        }
        (Value::Bytes(b), Value::UInt(idx)) => {
            b.get(*idx as usize).map(|byte| Value::UInt(*byte as u64))
        }
        _ => None,
    }
}

// ============================================================================
// Execution
// ============================================================================

/// Execute a named function in a compiled program.
/// Execute a named function (convenience — does HashMap lookup).
pub fn execute(
    program: &CompiledProgram,
    vm: &mut VM,
    func_name: &str,
    args: &[Value],
) -> Result<Option<Value>, ExecError> {
    let func_idx = program
        .func_index
        .get(func_name)
        .ok_or(ExecError::StackOverflow)?; // TODO: proper "function not found" error
    execute_by_index(program, vm, *func_idx, args)
}

/// Execute a function by resolved index (no lookup — hot path).
pub fn execute_by_index(
    program: &CompiledProgram,
    vm: &mut VM,
    func_idx: usize,
    args: &[Value],
) -> Result<Option<Value>, ExecError> {
    let arg_pairs: Vec<(Value, bool)> = args.iter().map(|v| (v.clone(), false)).collect();

    match execute_function(program, vm, func_idx, &arg_pairs)? {
        Action::Return(val) => Ok(val),
        Action::Exit(_val) => Ok(None), // TODO: propagate exit to driver
        Action::Continue | Action::NextBlock(_) => unreachable!(),
    }
}

// ============================================================================
// Intrinsic Runtime Execution
// ============================================================================

/// Execute an intrinsic operation at runtime.
///
/// Returns `Some(value)` on success, `None` for undefined (type mismatch,
/// overflow, out-of-bounds, etc.)
fn exec_intrinsic(
    op: IntrinsicOp,
    arg_slots: &[usize],
    vm: &mut VM,
) -> Result<Option<Value>, ExecError> {
    match op {
        // -- Arithmetic --
        IntrinsicOp::Add => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => a.checked_add(*b).map(Value::UInt),
                (Some(Value::Int(a)), Some(Value::Int(b))) => a.checked_add(*b).map(Value::Int),
                (Some(Value::Float(a)), Some(Value::Float(b))) => {
                    Float::new(a.get() + b.get()).map(Value::Float)
                }
                (Some(Value::UInt(a)), Some(Value::Int(b))) => i64::try_from(*a)
                    .ok()
                    .and_then(|a| a.checked_add(*b))
                    .map(Value::Int),
                (Some(Value::Int(a)), Some(Value::UInt(b))) => i64::try_from(*b)
                    .ok()
                    .and_then(|b| a.checked_add(b))
                    .map(Value::Int),
                (Some(Value::UInt(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 + b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::UInt(b))) => {
                    Float::new(a.get() + *b as f64).map(Value::Float)
                }
                (Some(Value::Int(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 + b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::Int(b))) => {
                    Float::new(a.get() + *b as f64).map(Value::Float)
                }
                _ => None,
            })
        }
        IntrinsicOp::Sub => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => a.checked_sub(*b).map(Value::UInt),
                (Some(Value::Int(a)), Some(Value::Int(b))) => a.checked_sub(*b).map(Value::Int),
                (Some(Value::Float(a)), Some(Value::Float(b))) => {
                    Float::new(a.get() - b.get()).map(Value::Float)
                }
                (Some(Value::UInt(a)), Some(Value::Int(b))) => i64::try_from(*a)
                    .ok()
                    .and_then(|a| a.checked_sub(*b))
                    .map(Value::Int),
                (Some(Value::Int(a)), Some(Value::UInt(b))) => i64::try_from(*b)
                    .ok()
                    .and_then(|b| a.checked_sub(b))
                    .map(Value::Int),
                (Some(Value::UInt(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 - b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::UInt(b))) => {
                    Float::new(a.get() - *b as f64).map(Value::Float)
                }
                (Some(Value::Int(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 - b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::Int(b))) => {
                    Float::new(a.get() - *b as f64).map(Value::Float)
                }
                _ => None,
            })
        }
        IntrinsicOp::Mul => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => a.checked_mul(*b).map(Value::UInt),
                (Some(Value::Int(a)), Some(Value::Int(b))) => a.checked_mul(*b).map(Value::Int),
                (Some(Value::Float(a)), Some(Value::Float(b))) => {
                    Float::new(a.get() * b.get()).map(Value::Float)
                }
                (Some(Value::UInt(a)), Some(Value::Int(b))) => i64::try_from(*a)
                    .ok()
                    .and_then(|a| a.checked_mul(*b))
                    .map(Value::Int),
                (Some(Value::Int(a)), Some(Value::UInt(b))) => i64::try_from(*b)
                    .ok()
                    .and_then(|b| a.checked_mul(b))
                    .map(Value::Int),
                (Some(Value::UInt(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 * b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::UInt(b))) => {
                    Float::new(a.get() * *b as f64).map(Value::Float)
                }
                (Some(Value::Int(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 * b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::Int(b))) => {
                    Float::new(a.get() * *b as f64).map(Value::Float)
                }
                _ => None,
            })
        }
        IntrinsicOp::Div => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => a.checked_div(*b).map(Value::UInt),
                (Some(Value::Int(a)), Some(Value::Int(b))) => a.checked_div(*b).map(Value::Int),
                (Some(Value::Float(a)), Some(Value::Float(b))) => {
                    Float::new(a.get() / b.get()).map(Value::Float)
                }
                (Some(Value::UInt(a)), Some(Value::Int(b))) => i64::try_from(*a)
                    .ok()
                    .and_then(|a| a.checked_div(*b))
                    .map(Value::Int),
                (Some(Value::Int(a)), Some(Value::UInt(b))) => i64::try_from(*b)
                    .ok()
                    .and_then(|b| a.checked_div(b))
                    .map(Value::Int),
                (Some(Value::UInt(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 / b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::UInt(b))) => {
                    Float::new(a.get() / *b as f64).map(Value::Float)
                }
                (Some(Value::Int(a)), Some(Value::Float(b))) => {
                    Float::new(*a as f64 / b.get()).map(Value::Float)
                }
                (Some(Value::Float(a)), Some(Value::Int(b))) => {
                    Float::new(a.get() / *b as f64).map(Value::Float)
                }
                _ => None,
            })
        }
        IntrinsicOp::Mod => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => a.checked_rem(*b).map(Value::UInt),
                (Some(Value::Int(a)), Some(Value::Int(b))) => a.checked_rem(*b).map(Value::Int),
                (Some(Value::Float(a)), Some(Value::Float(b))) => {
                    Float::new(a.get() % b.get()).map(Value::Float)
                }
                (Some(Value::UInt(a)), Some(Value::Int(b))) => i64::try_from(*a)
                    .ok()
                    .and_then(|a| a.checked_rem(*b))
                    .map(Value::Int),
                (Some(Value::Int(a)), Some(Value::UInt(b))) => i64::try_from(*b)
                    .ok()
                    .and_then(|b| a.checked_rem(b))
                    .map(Value::Int),
                _ => None,
            })
        }
        IntrinsicOp::Neg => {
            let a = vm.local(arg_slots[0]);
            Ok(match a {
                Some(Value::Int(a)) => a.checked_neg().map(Value::Int),
                Some(Value::Float(a)) => Float::new(-a.get()).map(Value::Float),
                Some(Value::UInt(a)) => i64::try_from(*a)
                    .ok()
                    .and_then(|v| v.checked_neg())
                    .map(Value::Int),
                _ => None,
            })
        }

        // -- Comparison --
        IntrinsicOp::Eq => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(a), Some(b)) => Some(Value::Bool(a == b)),
                _ => None,
            })
        }
        IntrinsicOp::Lt => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => Some(Value::Bool(a < b)),
                (Some(Value::Int(a)), Some(Value::Int(b))) => Some(Value::Bool(a < b)),
                (Some(Value::Float(a)), Some(Value::Float(b))) => {
                    Some(Value::Bool(a.get() < b.get()))
                }
                (Some(Value::UInt(a)), Some(Value::Int(b))) => {
                    Some(Value::Bool((*a as i128) < (*b as i128)))
                }
                (Some(Value::Int(a)), Some(Value::UInt(b))) => {
                    Some(Value::Bool((*a as i128) < (*b as i128)))
                }
                (Some(Value::UInt(a)), Some(Value::Float(b))) => {
                    Some(Value::Bool((*a as f64) < b.get()))
                }
                (Some(Value::Float(a)), Some(Value::UInt(b))) => {
                    Some(Value::Bool(a.get() < (*b as f64)))
                }
                (Some(Value::Int(a)), Some(Value::Float(b))) => {
                    Some(Value::Bool((*a as f64) < b.get()))
                }
                (Some(Value::Float(a)), Some(Value::Int(b))) => {
                    Some(Value::Bool(a.get() < (*b as f64)))
                }
                _ => None,
            })
        }

        // -- Logical --
        IntrinsicOp::Not => {
            let a = vm.local(arg_slots[0]);
            Ok(match a {
                Some(Value::Bool(b)) => Some(Value::Bool(!b)),
                _ => None,
            })
        }

        // -- Bitwise --
        IntrinsicOp::BitAnd => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => Some(Value::UInt(a & b)),
                _ => None,
            })
        }
        IntrinsicOp::BitOr => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => Some(Value::UInt(a | b)),
                _ => None,
            })
        }
        IntrinsicOp::BitXor => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => Some(Value::UInt(a ^ b)),
                _ => None,
            })
        }
        IntrinsicOp::BitNot => {
            let a = vm.local(arg_slots[0]);
            Ok(match a {
                Some(Value::UInt(a)) => Some(Value::UInt(!a)),
                _ => None,
            })
        }
        IntrinsicOp::Shl => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => {
                    Some(Value::UInt(a.wrapping_shl(*b as u32)))
                }
                _ => None,
            })
        }
        IntrinsicOp::Shr => {
            let (a, b) = get_two(arg_slots, vm);
            Ok(match (a, b) {
                (Some(Value::UInt(a)), Some(Value::UInt(b))) => {
                    Some(Value::UInt(a.wrapping_shr(*b as u32)))
                }
                _ => None,
            })
        }
        IntrinsicOp::BitTest => {
            let (x, b) = get_two(arg_slots, vm);
            Ok(match (x, b) {
                (Some(Value::UInt(x)), Some(Value::UInt(b))) => {
                    if *b >= 64 {
                        None
                    } else {
                        Some(Value::Bool((x >> b) & 1 == 1))
                    }
                }
                _ => None,
            })
        }
        IntrinsicOp::BitSet => {
            let (x, b) = get_two(arg_slots, vm);
            let v = vm.local(arg_slots[2]);
            Ok(match (x, b, v) {
                (Some(Value::UInt(x)), Some(Value::UInt(b)), Some(Value::Bool(v))) => {
                    if *b >= 64 {
                        None
                    } else if *v {
                        Some(Value::UInt(x | (1 << b)))
                    } else {
                        Some(Value::UInt(x & !(1 << b)))
                    }
                }
                _ => None,
            })
        }

        // -- Collection --
        IntrinsicOp::Len => {
            let a = vm.local(arg_slots[0]);
            Ok(match a {
                Some(Value::Text(s)) => Some(Value::UInt(s.chars().count() as u64)),
                Some(Value::Bytes(b)) => Some(Value::UInt(b.len() as u64)),
                Some(Value::Array(arr)) => Some(Value::UInt(arr.len() as u64)),
                Some(Value::Map(map)) => Some(Value::UInt(map.len() as u64)),
                _ => None,
            })
        }
        IntrinsicOp::MakeArray => {
            let elems: Vec<Value> = arg_slots
                .iter()
                .filter_map(|s| vm.local(*s).cloned())
                .collect();
            let arr = HeapVal::new(elems, vm.heap())?;
            Ok(Some(Value::Array(arr)))
        }
        IntrinsicOp::MakeMap => {
            if !arg_slots.len().is_multiple_of(2) {
                return Ok(None);
            }
            let map: IndexMap<Value, Value> = arg_slots
                .chunks(2)
                .filter_map(|pair| {
                    let k = vm.local(pair[0]).cloned()?;
                    let v = vm.local(pair[1]).cloned()?;
                    Some((k, v))
                })
                .collect();
            let heap_map = HeapVal::new(map, vm.heap())?;
            Ok(Some(Value::Map(heap_map)))
        }

        // -- Sequence (runtime-only, not yet implemented) --
        IntrinsicOp::MakeSeq | IntrinsicOp::ArraySeq => {
            // TODO: implement when Sequence runtime is ready
            Ok(None)
        }
    }
}

/// Helper: get two values from slots
fn get_two<'a>(slots: &[usize], vm: &'a VM) -> (Option<&'a Value>, Option<&'a Value>) {
    (vm.local(slots[0]), vm.local(slots[1]))
}

/// Execute a compiled function by index.
fn execute_function(
    program: &CompiledProgram,
    vm: &mut VM,
    func_idx: usize,
    args: &[(Value, bool)],
) -> Result<Action, ExecError> {
    let func = &program.functions[func_idx];

    // Set up call frame
    // return_slot = None for now — caller reads return value from Action
    vm.call(func.frame_size, None)?;

    // Bind parameters
    for (i, (val, by_ref)) in args.iter().enumerate() {
        if i < func.param_count {
            // Push arg to a temporary location, then bind
            let param_offset = i + 1; // slot 0 = Frame
            if *by_ref {
                // by-ref: for now, just copy (TODO: proper ref binding)
                vm.set_local(param_offset, val.clone());
            } else {
                vm.set_local(param_offset, val.clone());
            }
        }
    }

    // Execute: single flat loop with program counter
    let mut pc = func.block_starts[func.entry];

    loop {
        match (func.steps[pc])(vm, program)? {
            Action::Continue => pc += 1,
            Action::NextBlock(idx) => pc = func.block_starts[idx],
            Action::Return(val) => {
                vm.ret();
                return Ok(Action::Return(val));
            }
            Action::Exit(val) => {
                vm.ret();
                return Ok(Action::Exit(val));
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins;

    /// Helper: compile source and execute a named function
    fn run(source: &str, func_name: &str, args: &[Value]) -> Result<Option<Value>, String> {
        let builtins = builtins::standard_builtins();
        let (program, diagnostics) =
            crate::compile(source, &builtins).map_err(|d| format!("compilation failed: {}", d))?;

        if diagnostics.has_warnings() {
            eprintln!("warnings: {}", diagnostics);
        }

        let mut vm = VM::new();
        program
            .call(&mut vm, func_name, args)
            .map_err(|e| format!("exec error: {}", e))
    }

    /// Helper: compile and run, expecting a Value back
    fn run_expect(source: &str, func_name: &str) -> Value {
        run(source, func_name, &[])
            .expect("should not error")
            .expect("should return a value")
    }

    // ========================================================================
    // Basic Execution
    // ========================================================================

    #[test]
    fn test_return_constant() {
        let val = run_expect("fn test() { return 42; }", "test");
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_return_bool() {
        let val = run_expect("fn test() { return true; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_return_no_value() {
        let result = run("fn test() { return; }", "test", &[]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_implicit_return() {
        // Final expression without semicolon is the return value
        let val = run_expect("fn test() { 99 }", "test");
        assert_eq!(val, Value::UInt(99));
    }

    // ========================================================================
    // Arithmetic (binary builtins)
    // ========================================================================

    #[test]
    fn test_addition() {
        let val = run_expect("fn test() { return 1 + 2; }", "test");
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_arithmetic_expression() {
        let val = run_expect("fn test() { return (10 - 3) * 2; }", "test");
        assert_eq!(val, Value::UInt(14));
    }

    #[test]
    fn test_comparison() {
        let val = run_expect("fn test() { return 5 > 3; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_equality() {
        let val = run_expect("fn test() { return 42 == 42; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    // ========================================================================
    // Variables
    // ========================================================================

    #[test]
    fn test_let_binding() {
        let val = run_expect(
            "fn test() { let x = 10; let y = 20; return x + y; }",
            "test",
        );
        assert_eq!(val, Value::UInt(30));
    }

    #[test]
    fn test_variable_reassignment() {
        let val = run_expect("fn test() { let x = 1; x = x + 10; return x; }", "test");
        assert_eq!(val, Value::UInt(11));
    }

    // ========================================================================
    // Control Flow
    // ========================================================================

    #[test]
    fn test_if_true() {
        // Implicit return: if-expression is the final expression (no semicolon)
        let val = run_expect("fn test() { if true { 1 } else { 2 } }", "test");
        assert_eq!(val, Value::UInt(1));
    }

    #[test]
    fn test_if_false() {
        let val = run_expect("fn test() { if false { 1 } else { 2 } }", "test");
        assert_eq!(val, Value::UInt(2));
    }

    #[test]
    fn test_if_with_comparison() {
        let val = run_expect(
            "fn test() { let x = 10; if x > 5 { 1 } else { 0 } }",
            "test",
        );
        assert_eq!(val, Value::UInt(1));
    }

    // ========================================================================
    // Loops
    // ========================================================================

    #[test]
    fn test_while_loop() {
        let val = run_expect(
            r#"
            fn test() {
                let sum = 0;
                let i = 0;
                while i < 5 {
                    sum = sum + i;
                    i = i + 1;
                }
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(10)); // 0+1+2+3+4
    }

    #[test]
    fn test_loop_break() {
        let val = run_expect(
            r#"
            fn test() {
                let i = 0;
                loop {
                    if i >= 3 {
                        break;
                    }
                    i = i + 1;
                }
                return i;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_loop_break_with_value() {
        let val = run_expect(
            r#"
            fn test() {
                let result = loop {
                    break 42;
                };
                return result;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(42));
    }

    // ========================================================================
    // Functions
    // ========================================================================

    #[test]
    fn test_function_call() {
        let val = run_expect(
            r#"
            fn add(a, b) { return a + b; }
            fn test() { return add(3, 4); }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(7));
    }

    #[test]
    fn test_recursive_function() {
        let val = run_expect(
            r#"
            fn factorial(n) {
                if n <= 1 { return 1; }
                return n * factorial(n - 1);
            }
            fn test() { return factorial(5); }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(120));
    }

    // ========================================================================
    // Constants
    // ========================================================================

    #[test]
    fn test_const_binding() {
        let val = run_expect(
            r#"
            const MAX = 100;
            fn test() { return MAX; }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(100));
    }

    // ========================================================================
    // Short-circuit logic
    // ========================================================================

    #[test]
    fn test_short_circuit_and() {
        let val = run_expect("fn test() { return true && false; }", "test");
        assert_eq!(val, Value::Bool(false));
    }

    #[test]
    fn test_short_circuit_or() {
        let val = run_expect("fn test() { return false || true; }", "test");
        assert_eq!(val, Value::Bool(true));
    }

    // ========================================================================
    // Builtins
    // ========================================================================

    #[test]
    fn test_len() {
        let val = run_expect(r#"fn test() { let a = [1, 2, 3]; return len(a); }"#, "test");
        assert_eq!(val, Value::UInt(3));
    }

    #[test]
    fn test_negation() {
        let val = run_expect("fn test() { return !true; }", "test");
        assert_eq!(val, Value::Bool(false));
    }

    // ========================================================================
    // Match / Pattern Matching
    // ========================================================================

    #[test]
    fn test_match_literal() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 2;
                match x {
                    1 => { return 10; },
                    2 => { return 20; },
                    3 => { return 30; },
                    _ => { return 0; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(20));
    }

    #[test]
    fn test_match_wildcard() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 99;
                match x {
                    1 => { return 10; },
                    _ => { return 42; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_match_type_pattern() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 42;
                match x {
                    Bool(b) => { return 0; },
                    UInt(n) => { return n; },
                    _ => { return 99; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(42));
    }

    #[test]
    fn test_match_with_guard() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 15;
                match x {
                    UInt(n) if n > 10 => { return 1; },
                    UInt(n) => { return 2; },
                    _ => { return 3; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(1));
    }

    #[test]
    fn test_match_guard_fails() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 5;
                match x {
                    UInt(n) if n > 10 => { return 1; },
                    UInt(n) => { return 2; },
                    _ => { return 3; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(2));
    }

    // ========================================================================
    // If-Let / If-With Patterns
    // ========================================================================

    #[test]
    fn test_if_let_binding() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 42;
                if let y = x {
                    return y + 1;
                }
                return 0;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(43));
    }

    #[test]
    fn test_if_let_type_pattern() {
        let val = run_expect(
            r#"
            fn test() {
                let x = 42;
                if let UInt(n) = x {
                    return n + 10;
                }
                return 0;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(52));
    }

    // ========================================================================
    // Array Destructuring
    // ========================================================================

    #[test]
    fn test_let_array_destructure() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let [a, b, c] = arr;
                return a + b + c;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(60));
    }

    #[test]
    fn test_match_array_pattern() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2];
                match arr {
                    [a, b] => { return a + b; },
                    _ => { return 0; },
                }
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(3));
    }

    // ========================================================================
    // For Loop Execution
    // ========================================================================

    #[test]
    fn test_for_array_sum() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let sum = 0;
                for x in arr {
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(60));
    }

    #[test]
    fn test_for_array_with_index() {
        // Pair binding: i = index, x = element
        let val = run_expect(
            r#"
            fn test() {
                let arr = [10, 20, 30];
                let result = 0;
                for i, x in arr {
                    result = result + i + x;
                };
                return result;
            }
            "#,
            "test",
        );
        // (0+10) + (1+20) + (2+30) = 63
        assert_eq!(val, Value::UInt(63));
    }

    #[test]
    fn test_for_with_break() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2, 3, 4, 5];
                let sum = 0;
                for x in arr {
                    if x > 3 { break; };
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        // 1 + 2 + 3 = 6 (stops before 4)
        assert_eq!(val, Value::UInt(6));
    }

    #[test]
    fn test_for_with_continue() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2, 3, 4, 5];
                let sum = 0;
                for x in arr {
                    if x == 3 { continue; };
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        // 1 + 2 + 4 + 5 = 12 (skips 3)
        assert_eq!(val, Value::UInt(12));
    }

    #[test]
    fn test_for_empty_array() {
        let val = run_expect(
            r#"
            fn test() {
                let arr = [];
                let count = 0;
                for x in arr {
                    count = count + 1;
                };
                return count;
            }
            "#,
            "test",
        );
        assert_eq!(val, Value::UInt(0));
    }

    #[test]
    fn test_for_nested() {
        let val = run_expect(
            r#"
            fn test() {
                let a = [1, 2];
                let b = [10, 20];
                let sum = 0;
                for x in a {
                    for y in b {
                        sum = sum + x * y;
                    };
                };
                return sum;
            }
            "#,
            "test",
        );
        // 1*10 + 1*20 + 2*10 + 2*20 = 10 + 20 + 20 + 40 = 90
        assert_eq!(val, Value::UInt(90));
    }

    #[test]
    fn test_for_let_binding() {
        // for let x — by-value, mutations don't affect source
        let val = run_expect(
            r#"
            fn test() {
                let arr = [1, 2, 3];
                let sum = 0;
                for let x in arr {
                    x = x * 10;
                    sum = sum + x;
                };
                return sum;
            }
            "#,
            "test",
        );
        // 10 + 20 + 30 = 60
        assert_eq!(val, Value::UInt(60));
    }
}
