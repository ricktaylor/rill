//! IR-to-Closure Compilation and Execution
//!
//! Compiles the SSA-form IR into closure-threaded code for execution.
//! Each IR instruction becomes a Rust closure that captures its operands
//! (slot offsets, resolved extern function pointers, constant values).
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
//! - Externs resolved once at compile time (no runtime HashMap lookup)
//! - VarIds mapped to stack slot offsets at compile time
//! - Loops use iterative block dispatch (no Rust stack growth)
//! - User function calls use recursive inline loops (bounded by VM stack limit)
//! - Future: tail-call optimization can convert recursive calls to jumps

mod exec;
mod specialize;
mod terminator;

#[cfg(test)]
mod tests;

use crate::diagnostics::Diagnostics;
use crate::exec::{ExecError, Float, HeapVal, SeqState, VM, Value};
use crate::externs::{ExecResult, ExternImpl, ExternRegistry};
use crate::ir::{
    BasicBlock, BlockId, Function, Instruction, IntrinsicOp, IrProgram, Literal, MatchPattern,
    Terminator, VarId,
};
use crate::opt::TypeAnalysis;
use crate::ssa::slot_alloc::SlotAlloc;
use crate::types::{BaseType, ConvertMode, NumericType, TypeSet};
use indexmap::IndexMap;
use std::collections::HashMap;

// Re-export submodule items used internally by compile_instruction
use exec::*;
use specialize::*;
use terminator::*;

// ============================================================================
// Compiled Types
// ============================================================================

/// A compiled program ready for execution.
pub struct CompiledProgram {
    functions: Vec<CompiledFunction>,
    /// Function name → index into `functions`
    pub func_index: HashMap<String, usize>,
    /// Number of file-scope global slots (0..N of the VM stack), reserved by
    /// `VM::exec` before any user call.
    pub global_count: usize,
    /// Index of the synthetic `__init__` function (if the program has globals).
    /// Run by `VM::exec` to populate the global slots in source order.
    pub init_func: Option<usize>,
    /// Warnings from compilation (unused functions, etc.)
    pub warnings: Diagnostics,
}

/// A compiled function — flat array of step closures with block offsets.
pub struct CompiledFunction {
    /// All step closures for all blocks, flattened into a single contiguous array.
    /// Block boundaries are recorded in `block_starts`.
    pub steps: Vec<Step>,
    /// Offset into `steps` where each block begins.
    /// `block_starts[i]` is the index of the first step in block i.
    pub block_starts: Vec<usize>,
    pub entry: usize, // index into block_starts
    pub frame_size: usize,
    pub param_count: usize,
}

impl CompiledProgram {
    /// Frame size (allocated physical slot count) of a function by name.
    pub fn function_frame_size(&self, name: &str) -> Option<usize> {
        self.func_index
            .get(name)
            .and_then(|&i| self.functions.get(i))
            .map(|f| f.frame_size)
    }
}

/// A step closure. Captures operands, operates on VM.
/// Instructions return Continue; terminators return NextBlock/Return/Exit.
pub type Step = Box<dyn Fn(&mut VM, &CompiledProgram) -> Result<Action, ExecError>>;

/// Result of executing a step.
pub enum Action {
    /// Continue to the next step in this block
    Continue,
    /// Jump to another block
    NextBlock(usize),
    /// Return from function. `Value::Undefined` for void/failed returns.
    Return(Value),
    /// Hard exit to driver (diverging extern)
    Exit(Value),
}

// ============================================================================
// Slot Mapping
// ============================================================================
//
// Physical stack slots are assigned per function by `SlotAlloc` (built in
// `compile_function`), which coalesces non-interfering VarIds. `alloc.slot(var)`
// replaces the old identity `var.0` mapping.

/// Maps BlockId to index in the compiled blocks array.
fn build_block_map(blocks: &[BasicBlock]) -> HashMap<BlockId, usize> {
    blocks
        .iter()
        .enumerate()
        .map(|(idx, b)| (b.id, idx))
        .collect()
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
    externs: &ExternRegistry,
) -> Result<CompiledProgram, Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    // Build user function index
    let mut func_index: HashMap<String, usize> = HashMap::new();
    for (idx, ir_func) in ir.functions.iter().enumerate() {
        func_index.insert(ir_func.name.to_string(), idx);
    }

    // Link phase: resolve all Call references. Whole-program dead-import
    // elimination already ran at merge time, so every function here is either a
    // root entry point or reachable from one.
    let link_map = link_functions(ir, externs, &func_index, &mut diagnostics);

    if diagnostics.has_errors() {
        return Err(diagnostics);
    }

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

    let init_func = func_index.get("__init__").copied();

    let mut program = CompiledProgram {
        functions: compiled_functions,
        func_index,
        global_count: ir.global_count,
        init_func,
        warnings: Diagnostics::new(),
    };

    // Attach warnings to the program for the caller
    program.warnings = diagnostics;

    Ok(program)
}

/// Resolution of a function call — determined at link time.
#[derive(Clone)]
pub enum CallTarget {
    /// Native extern — function pointer resolved at compile time.
    /// Includes optional type-specialized variants for monomorphic dispatch.
    Extern {
        generic: crate::externs::ExternFn,
        /// Variants: (param TypeSets, return TypeSet, specialized fn pointer)
        variants: Vec<(Vec<TypeSet>, TypeSet, crate::externs::ExternFn)>,
    },
    /// User-defined function — index into CompiledProgram.functions.
    /// Copy-out for by-ref params is derived from Reload instructions in the IR.
    UserFunction(usize),
}

/// Map from qualified function name to its resolved target.
pub type LinkMap = HashMap<String, CallTarget>;

/// Link phase: resolve all function references, erroring on any that don't.
fn link_functions(
    ir: &IrProgram,
    externs: &ExternRegistry,
    func_index: &HashMap<String, usize>,
    diagnostics: &mut Diagnostics,
) -> LinkMap {
    let mut link_map = LinkMap::new();

    // Pre-populate with all externs
    for (name, def) in externs.iter() {
        if let ExternImpl::Native(f) = &def.implementation {
            let variants: Vec<(Vec<TypeSet>, TypeSet, crate::externs::ExternFn)> = def
                .variants
                .iter()
                .filter_map(|v| {
                    if let ExternImpl::Native(vf) = &v.implementation {
                        Some((v.param_types.clone(), v.returns, *vf))
                    } else {
                        None
                    }
                })
                .collect();
            link_map.insert(
                name.clone(),
                CallTarget::Extern {
                    generic: *f,
                    variants,
                },
            );
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
                    if !link_map.contains_key(&qname) {
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

    // Slot allocation: coalesce non-interfering VarIds onto shared physical
    // slots. `alloc.slot(var)` is the storage offset; `frame_size` is the
    // number of slots needed. Built on the original IR so type analysis below
    // keeps per-VarId precision.
    let alloc = SlotAlloc::build(func, &crate::ir::cfg::block_map(func));
    let frame_size = alloc.frame_size();

    // Type analysis for specialization — when both operands of an arithmetic
    // op are provably the same type, the compiler emits a direct closure
    // instead of the 10-way type dispatch.
    let types = crate::opt::analyze_types(func, None);

    // First pass: compile all blocks, collecting phi metadata
    let mut blocks = Vec::new();
    #[allow(clippy::type_complexity)]
    let mut pending_phis: Vec<(usize, usize, Vec<(usize, usize)>)> = Vec::new();
    // pending_phis: (dest_slot, join_block_idx, [(pred_block_idx, src_slot)])

    for ir_block in &func.blocks {
        let join_idx = block_map[&ir_block.id];

        // Collect phis from this block
        for inst in &ir_block.instructions {
            if let Instruction::Phi { dest, sources } = &inst.node {
                let d = alloc.slot(*dest);
                let compiled_sources: Vec<(usize, usize)> = sources
                    .iter()
                    .filter_map(|(block_id, var_id)| {
                        block_map.get(block_id).map(|&idx| (idx, alloc.slot(*var_id)))
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

        blocks.push(compile_block(
            ir_block, &block_map, link_map, &types, frame_size, &alloc,
        )?);
    }

    // Second pass: resolve phis by inserting copies into predecessor blocks.
    //
    // Multiple phis at the same join block may insert copies into the same
    // predecessor. These copies must behave as parallel assignments — all
    // sources are read before any dest is written. We group copies by
    // predecessor and emit a single closure that reads all sources into
    // temporaries, then writes all dests.
    {
        // Group: pred_block_idx → [(dest_slot, src_slot)]
        let mut copies_per_pred: std::collections::HashMap<usize, Vec<(usize, usize)>> =
            std::collections::HashMap::new();

        for (dest_slot, _join_idx, sources) in pending_phis {
            for (pred_block_idx, src_slot) in sources {
                if src_slot != dest_slot {
                    copies_per_pred
                        .entry(pred_block_idx)
                        .or_default()
                        .push((dest_slot, src_slot));
                }
            }
        }

        for (pred_block_idx, copies) in copies_per_pred {
            let block = &mut blocks[pred_block_idx];
            let insert_pos = if block.is_empty() { 0 } else { block.len() - 1 };
            block.insert(
                insert_pos,
                Box::new(move |vm: &mut VM, _prog| {
                    // Read all sources first (parallel semantics), then move
                    // each into its dest — the read already owns a clone, so
                    // no second clone is needed on write.
                    let vals: Vec<_> = copies.iter().map(|&(_, s)| vm.local(s).clone()).collect();
                    for (&(d, _), val) in copies.iter().zip(vals) {
                        vm.set_local(d, val);
                    }
                    Ok(Action::Continue)
                }),
            );
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
    types: &TypeAnalysis,
    frame_size: usize,
    alloc: &SlotAlloc,
) -> Result<Vec<Step>, ExecError> {
    let mut steps: Vec<Step> = Vec::new();

    for spanned_inst in &block.instructions {
        match &spanned_inst.node {
            // Phis are handled in compile_function's second pass —
            // copies are inserted into predecessor blocks
            Instruction::Phi { .. } => {}
            inst => {
                if let Some(step) =
                    compile_instruction(inst, block_map, link_map, types, block.id, alloc)?
                {
                    steps.push(step);
                }
            }
        }
    }

    // Terminator is the last step in the block
    steps.push(compile_terminator(
        &block.terminator,
        block_map,
        types,
        block.id,
        frame_size,
        alloc,
    )?);

    Ok(steps)
}

fn compile_instruction(
    inst: &Instruction,
    _block_map: &HashMap<BlockId, usize>,
    link_map: &LinkMap,
    types: &TypeAnalysis,
    block_id: BlockId,
    alloc: &SlotAlloc,
) -> Result<Option<Step>, ExecError> {
    Ok(Some(match inst {
        Instruction::Const { dest, value } => {
            let d = alloc.slot(*dest);
            // Pre-compute scalar values at compile time — no runtime match needed.
            // Only Text/Bytes require runtime heap allocation.
            match value {
                Literal::Bool(b) => {
                    let v = Value::Bool(*b);
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, v.clone());
                        Ok(Action::Continue)
                    })
                }
                Literal::UInt(n) => {
                    let v = Value::UInt(*n);
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, v.clone());
                        Ok(Action::Continue)
                    })
                }
                Literal::Int(n) => {
                    let v = Value::Int(*n);
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, v.clone());
                        Ok(Action::Continue)
                    })
                }
                Literal::Float(f) => {
                    match Float::new(*f) {
                        Some(float) => {
                            let v = Value::Float(float);
                            Box::new(move |vm: &mut VM, _prog| {
                                vm.set_local(d, v.clone());
                                Ok(Action::Continue)
                            })
                        }
                        None => {
                            // Non-finite literal → undefined
                            Box::new(move |vm: &mut VM, _prog| {
                                vm.set_local(d, Value::Undefined);
                                Ok(Action::Continue)
                            })
                        }
                    }
                }
                Literal::Text(s) => {
                    // Intern: allocate on first execution, reuse Rc clone after.
                    let text = s.clone();
                    let cache = std::cell::RefCell::new(None);
                    Box::new(move |vm: &mut VM, _prog| {
                        if cache.borrow().is_none() {
                            let v = Value::Text(HeapVal::new(text.clone(), vm.heap())?);
                            *cache.borrow_mut() = Some(v);
                        }
                        let val = cache.borrow().as_ref().unwrap().clone();
                        vm.set_local(d, val);
                        Ok(Action::Continue)
                    })
                }
                Literal::Bytes(b) => {
                    let bytes = b.clone();
                    let cache = std::cell::RefCell::new(None);
                    Box::new(move |vm: &mut VM, _prog| {
                        if cache.borrow().is_none() {
                            let v = Value::Bytes(HeapVal::new(bytes.clone(), vm.heap())?);
                            *cache.borrow_mut() = Some(v);
                        }
                        let val = cache.borrow().as_ref().unwrap().clone();
                        vm.set_local(d, val);
                        Ok(Action::Continue)
                    })
                }
                Literal::Undefined => Box::new(move |vm: &mut VM, _prog| {
                    vm.set_local(d, Value::Undefined);
                    Ok(Action::Continue)
                }),
            }
        }

        Instruction::Copy { dest, src } => {
            let d = alloc.slot(*dest);
            let s = alloc.slot(*src);
            if types.get(*src).is_some_and(|t| t.is_defined()) {
                Box::new(move |vm: &mut VM, _prog| {
                    let val = vm.local(s).clone();
                    vm.set_local(d, val);
                    Ok(Action::Continue)
                })
            } else {
                Box::new(move |vm: &mut VM, _prog| {
                    vm.set_local(d, vm.local(s).clone());
                    Ok(Action::Continue)
                })
            }
        }

        Instruction::Index { dest, base, key } => {
            let d = alloc.slot(*dest);
            let b = alloc.slot(*base);
            let k = alloc.slot(*key);

            // Specialize based on known base type
            let base_type = types.get(*base).filter(|t| t.is_single());

            if base_type.is_some_and(|t| t.contains(BaseType::Array)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Value::Array(arr), Value::UInt(idx)) => arr.get(*idx as usize).cloned(),
                        (Value::Array(arr), Value::Int(idx)) if *idx >= 0 => {
                            arr.get(*idx as usize).cloned()
                        }
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local(d, Value::Undefined),
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Map)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Value::Map(map), key_val) => map.get(key_val).cloned(),
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local(d, Value::Undefined),
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Text)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Value::Text(s), Value::UInt(idx)) => {
                            s.chars().nth(*idx as usize).map(|c| Value::UInt(c as u64))
                        }
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local(d, Value::Undefined),
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Bytes)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (Value::Bytes(bytes), Value::UInt(idx)) => bytes
                            .get(*idx as usize)
                            .map(|byte| Value::UInt(*byte as u64)),
                        _ => None,
                    };
                    match result {
                        Some(val) => vm.set_local(d, val),
                        None => vm.set_local(d, Value::Undefined),
                    }
                    Ok(Action::Continue)
                })
            } else {
                // Unknown base: full runtime dispatch
                Box::new(move |vm: &mut VM, _prog| {
                    let result = match (vm.local(b), vm.local(k)) {
                        (base_val, key_val) if base_val.is_defined() => {
                            index_value(base_val, key_val)
                        }
                        _ => Value::Undefined,
                    };
                    vm.set_local(d, result);
                    Ok(Action::Continue)
                })
            }
        }

        Instruction::Call {
            dest,
            function,
            args,
        } => {
            let d = alloc.slot(*dest);
            let arg_slots: Vec<usize> = args.iter().map(|v| alloc.slot(*v)).collect();
            let func_name = function.qualified_name();

            // Resolve via link map (all references verified at link time)
            match link_map.get(&func_name).cloned() {
                Some(CallTarget::Extern { generic, variants }) => {
                    // Try to select a type-specialized variant at compile time
                    let f = if !variants.is_empty() {
                        let arg_types: Vec<TypeSet> = args
                            .iter()
                            .map(|a| types.get(*a).copied().unwrap_or(TypeSet::any()))
                            .collect();
                        variants
                            .iter()
                            .find(|(param_types, _, _)| {
                                param_types.len() == arg_types.len()
                                    && param_types.iter().zip(&arg_types).all(|(spec, actual)| {
                                        !actual.is_empty() && actual.difference(spec).is_empty()
                                    })
                            })
                            .map(|(_, _, vf)| *vf)
                            .unwrap_or(generic)
                    } else {
                        generic
                    };

                    let argc = arg_slots.len();
                    Box::new(move |vm: &mut VM, _prog| {
                        let caller_bp = vm.bp();
                        let frame_size = argc; // frame info on separate stack
                        vm.call(frame_size, None)?;

                        // Uniform shallow copy — Refs stay Refs, Vals stay Vals.
                        // The lowerer already emitted MakeRef for by-ref args.
                        for (i, &s) in arg_slots.iter().enumerate() {
                            vm.copy_slot_from(i, caller_bp + s);
                        }

                        let result = f(vm, argc);
                        vm.ret();

                        match result? {
                            ExecResult::Return(val) => vm.set_local(d, val),
                            ExecResult::Exit(val) => return Ok(Action::Exit(val)),
                        }
                        Ok(Action::Continue)
                    })
                }
                Some(CallTarget::UserFunction(func_idx)) => {
                    Box::new(move |vm: &mut VM, prog: &CompiledProgram| {
                        let func = &prog.functions[func_idx];

                        let caller_bp = vm.bp();
                        vm.call(func.frame_size, None)?;

                        // Uniform shallow copy — ref-agnostic. The lowerer
                        // already emitted MakeRef for by-ref args.
                        for (i, &s) in arg_slots.iter().enumerate() {
                            if i < func.param_count {
                                vm.copy_slot_from(i, caller_bp + s);
                            }
                        }

                        // Execute callee
                        let mut pc = func.block_starts[func.entry];
                        let result = loop {
                            match (func.steps[pc])(vm, prog)? {
                                Action::Continue => pc += 1,
                                Action::NextBlock(idx) => pc = func.block_starts[idx],
                                Action::Return(val) => {
                                    vm.ret();
                                    break val;
                                }
                                Action::Exit(val) => {
                                    vm.ret();
                                    return Ok(Action::Exit(val));
                                }
                            }
                        };
                        vm.set_local(d, result);
                        Ok(Action::Continue)
                    })
                }
                None => {
                    // Unresolved — link phase should have caught this.
                    // Emit undefined as fallback.
                    Box::new(move |vm: &mut VM, _prog| {
                        vm.set_local(d, Value::Undefined);
                        Ok(Action::Continue)
                    })
                }
            }
        }

        Instruction::Intrinsic { dest, op, args } => {
            let d = alloc.slot(*dest);
            let op = *op;
            let arg_slots: Vec<usize> = args.iter().map(|v| alloc.slot(*v)).collect();

            // Try type-specialized compilation for binary arithmetic.
            // If both operands are provably the same single numeric type,
            // emit a direct closure that skips the runtime type dispatch.
            if let Some(specialized) =
                try_specialize_binary(op, &arg_slots, d, args, types, block_id)
            {
                return Ok(Some(specialized));
            }

            // Try type-specialized compilation for Convert.
            // Target type and mode are part of the op variant.
            if let Some(specialized) =
                try_specialize_convert(op, &arg_slots, d, args, types, block_id)
            {
                return Ok(Some(specialized));
            }

            // Dispatch on op at compile time so each closure goes directly
            // to its operation's code — no runtime `match op` in exec_intrinsic.
            compile_intrinsic_dispatch(op, arg_slots, d)
        }

        Instruction::MakeAccessor { dest, base, key } => {
            let d = alloc.slot(*dest);
            let b = alloc.slot(*base);
            let k = alloc.slot(*key);
            // Create Slot::Accessor — a far pointer into a collection.
            // The base slot holds the collection, the key slot holds the index/key.
            // Reading through the Accessor does base[key].
            // Writing through the Accessor mutates the collection element.
            Box::new(move |vm: &mut VM, _prog| {
                let base_abs = vm.resolve(vm.bp() + b);
                let key_abs = vm.bp() + k;
                vm.set_accessor(d, base_abs, key_abs);
                Ok(Action::Continue)
            })
        }

        Instruction::MakeRef { dest, base } => {
            let d = alloc.slot(*dest);
            let b = alloc.slot(*base);
            // Whole-value reference: create a Slot::Ref to base's
            // ultimate target (path compression — resolve the chain
            // once here, not on every subsequent read/write).
            Box::new(move |vm: &mut VM, _prog| {
                let target = vm.resolve(vm.bp() + b);
                vm.set_local_ref(d, target);
                Ok(Action::Continue)
            })
        }

        Instruction::WriteRef { ref_var, value } => {
            let r = alloc.slot(*ref_var);
            let v = alloc.slot(*value);
            // Write through the ref_var's slot. The VM's set_local resolves
            // through Slot::Ref (near pointer) and Slot::Accessor (far pointer)
            // automatically — no build_ref_map tracing needed.
            Box::new(move |vm: &mut VM, _prog| {
                vm.set_local(r, vm.local(v).clone());
                Ok(Action::Continue)
            })
        }

        Instruction::WriteAccessor { base, key, value } => {
            let b = alloc.slot(*base);
            let k = alloc.slot(*key);
            let v = alloc.slot(*value);
            let base_type = types.get(*base).filter(|t| t.is_single());

            if base_type.is_some_and(|t| t.contains(BaseType::Array)) {
                Box::new(move |vm: &mut VM, _prog| {
                    // Accept the same keys as the read path: UInt or Int >= 0
                    match vm.local(k) {
                        Value::UInt(idx) => {
                            let idx = *idx as usize;
                            let val = vm.local(v).clone();
                            let _ = vm.set_array_elem(vm.bp() + b, idx, val);
                        }
                        Value::Int(idx) if *idx >= 0 => {
                            let idx = *idx as usize;
                            let val = vm.local(v).clone();
                            let _ = vm.set_array_elem(vm.bp() + b, idx, val);
                        }
                        _ => {}
                    }
                    Ok(Action::Continue)
                })
            } else if base_type.is_some_and(|t| t.contains(BaseType::Map)) {
                Box::new(move |vm: &mut VM, _prog| {
                    let key_val = vm.local(k).clone();
                    let val = vm.local(v).clone();
                    let _ = vm.set_map_entry(vm.bp() + b, key_val, val);
                    Ok(Action::Continue)
                })
            } else {
                Box::new(move |vm: &mut VM, _prog| {
                    let key_val = vm.local(k).clone();
                    let val = vm.local(v).clone();
                    // Dispatch on BASE type, not key type
                    match vm.local(b) {
                        Value::Array(_) => match &key_val {
                            Value::UInt(idx) => {
                                let _ = vm.set_array_elem(vm.bp() + b, *idx as usize, val);
                            }
                            Value::Int(idx) if *idx >= 0 => {
                                let _ = vm.set_array_elem(vm.bp() + b, *idx as usize, val);
                            }
                            _ => {}
                        },
                        _ => {
                            let _ = vm.set_map_entry(vm.bp() + b, key_val, val);
                        }
                    }
                    Ok(Action::Continue)
                })
            }
        }

        Instruction::Append { dest, arr, value } => {
            let d = alloc.slot(*dest);
            let a = alloc.slot(*arr);
            let v = alloc.slot(*value);
            Box::new(move |vm: &mut VM, _prog| {
                let val = vm.local(v).clone();
                if val.is_defined() && vm.array_append(vm.bp() + a, val)? {
                    vm.set_local(d, vm.local(a).clone());
                } else {
                    vm.set_local(d, Value::Undefined);
                }
                Ok(Action::Continue)
            })
        }

        // Reload: read current slot value into a new slot (SSA barrier after mutation)
        Instruction::Reload { dest, src } => {
            let d = alloc.slot(*dest);
            let s = alloc.slot(*src);
            Box::new(move |vm: &mut VM, _prog| {
                vm.set_local(d, vm.local(s).clone());
                Ok(Action::Continue)
            })
        }

        // LoadGlobal: copy a global (absolute slot) into a local (bp-relative).
        Instruction::LoadGlobal {
            dest,
            slot: global_slot,
        } => {
            let d = alloc.slot(*dest);
            let g = *global_slot as usize;
            Box::new(move |vm: &mut VM, _prog| {
                let v = vm.get(g).cloned().unwrap_or(Value::Undefined);
                vm.set_local(d, v);
                Ok(Action::Continue)
            })
        }

        // StoreGlobal: write a local (bp-relative) into a global (absolute slot).
        Instruction::StoreGlobal {
            slot: global_slot,
            value,
        } => {
            let g = *global_slot as usize;
            let v = alloc.slot(*value);
            Box::new(move |vm: &mut VM, _prog| {
                let val = vm.local(v).clone();
                vm.set(g, val);
                Ok(Action::Continue)
            })
        }

        // Phi is handled separately in compile_block
        Instruction::Phi { .. } => return Ok(None),

        Instruction::Assign { .. } | Instruction::Read { .. } => {
            unreachable!("pre-SSA instruction; removed by mem2reg")
        }
    }))
}

// ============================================================================
// Execution
// ============================================================================

/// Execute a named function (convenience — does HashMap lookup).
///
/// Args should be pushed onto the VM stack before calling:
/// ```ignore
/// vm.push(Value::UInt(42))?;
/// let result = execute(&program, &mut vm, "func", 1)?;
/// ```
pub fn execute(
    program: &CompiledProgram,
    vm: &mut VM,
    func_name: &str,
    argc: usize,
) -> Result<Value, ExecError> {
    let func_idx = *program
        .func_index
        .get(func_name)
        .ok_or(ExecError::StackOverflow)?; // TODO: proper "function not found" error
    execute_by_index(program, vm, func_idx, argc)
}

/// Execute a function by resolved index (no lookup — hot path).
///
/// Args should be pushed onto the VM stack before calling.
pub fn execute_by_index(
    program: &CompiledProgram,
    vm: &mut VM,
    func_idx: usize,
    argc: usize,
) -> Result<Value, ExecError> {
    let func = &program.functions[func_idx];

    // Adopt pushed args into the call frame (zero allocation)
    vm.call_with_args(func.frame_size, argc)?;

    // Execute: single flat loop with program counter
    let mut pc = func.block_starts[func.entry];

    loop {
        match (func.steps[pc])(vm, program)? {
            Action::Continue => pc += 1,
            Action::NextBlock(idx) => pc = func.block_starts[idx],
            Action::Return(val) => {
                vm.ret();
                return Ok(val);
            }
            Action::Exit(val) => {
                vm.ret();
                return Ok(val);
            }
        }
    }
}
