//! Dead Code Elimination
//!
//! Removes instructions whose dest VarId is never used by any other
//! instruction or terminator. Iterates until stable (removing one dead
//! instruction may make its operands' definitions dead too).
//!
//! Respects side effects:
//! - `Call` is always kept (host builtins may be impure)
//! - `SetIndex`, `WriteRef`, `Drop` have no dest — always kept
//! - Everything else (Const, Copy, Undefined, Index, Intrinsic, Phi, MakeRef)
//!   is removed if its dest is unused

use super::{Function, Instruction, Terminator, VarId};
use std::collections::HashSet;

/// Eliminate dead instructions. Returns the number removed.
pub fn eliminate_dead_code(function: &mut Function) -> usize {
    let mut total = 0;

    // Iterate until stable — removing dead instructions may expose more
    loop {
        let used = collect_used_vars(function);
        let removed = remove_dead(function, &used);
        if removed == 0 {
            break;
        }
        total += removed;
    }

    total
}

/// Collect all VarIds that are used (read) anywhere in the function.
fn collect_used_vars(function: &Function) -> HashSet<VarId> {
    let mut used = HashSet::new();

    for block in &function.blocks {
        for inst in &block.instructions {
            collect_reads(&inst.node, &mut used);
        }
        collect_terminator_reads(&block.terminator, &mut used);
    }

    used
}

/// Collect VarIds read by an instruction (not the dest).
fn collect_reads(inst: &Instruction, used: &mut HashSet<VarId>) {
    match inst {
        Instruction::Const { .. } | Instruction::Undefined { .. } => {}

        Instruction::Copy { src, .. } => {
            used.insert(*src);
        }

        Instruction::Phi { sources, .. } => {
            for (_, var) in sources {
                used.insert(*var);
            }
        }

        Instruction::Index { base, key, .. } => {
            used.insert(*base);
            used.insert(*key);
        }

        Instruction::SetIndex { base, key, value } => {
            used.insert(*base);
            used.insert(*key);
            used.insert(*value);
        }

        Instruction::Intrinsic { args, .. } => {
            for arg in args {
                used.insert(*arg);
            }
        }

        Instruction::Call { args, .. } => {
            for arg in args {
                used.insert(arg.value);
            }
        }

        Instruction::MakeRef { base, key, .. } => {
            used.insert(*base);
            if let Some(k) = key {
                used.insert(*k);
            }
        }

        Instruction::WriteRef { ref_var, value } => {
            used.insert(*ref_var);
            used.insert(*value);
        }

        Instruction::Drop { vars } => {
            for v in vars {
                used.insert(*v);
            }
        }
    }
}

/// Collect VarIds read by a terminator.
fn collect_terminator_reads(term: &Terminator, used: &mut HashSet<VarId>) {
    match term {
        Terminator::If { condition, .. } => {
            used.insert(*condition);
        }
        Terminator::Match { value, .. } => {
            used.insert(*value);
        }
        Terminator::Guard { value, .. } => {
            used.insert(*value);
        }
        Terminator::Return { value: Some(v) } => {
            used.insert(*v);
        }
        Terminator::Exit { value } => {
            used.insert(*value);
        }
        Terminator::Jump { .. } | Terminator::Return { value: None } | Terminator::Unreachable => {}
    }
}

/// Is this instruction safe to remove when its dest is unused?
fn is_removable(inst: &Instruction) -> bool {
    match inst {
        // Pure instructions with a dest — safe to remove
        Instruction::Const { .. }
        | Instruction::Copy { .. }
        | Instruction::Undefined { .. }
        | Instruction::Index { .. }
        | Instruction::Intrinsic { .. }
        | Instruction::Phi { .. }
        | Instruction::MakeRef { .. } => true,

        // Call may have side effects — always keep
        Instruction::Call { .. } => false,

        // No dest — always keep (side effects)
        Instruction::SetIndex { .. } | Instruction::WriteRef { .. } | Instruction::Drop { .. } => {
            false
        }
    }
}

/// Get the dest VarId of an instruction, if it has one.
fn get_dest(inst: &Instruction) -> Option<VarId> {
    match inst {
        Instruction::Const { dest, .. }
        | Instruction::Copy { dest, .. }
        | Instruction::Undefined { dest }
        | Instruction::Index { dest, .. }
        | Instruction::Intrinsic { dest, .. }
        | Instruction::Phi { dest, .. }
        | Instruction::MakeRef { dest, .. }
        | Instruction::Call { dest, .. } => Some(*dest),

        Instruction::SetIndex { .. } | Instruction::WriteRef { .. } | Instruction::Drop { .. } => {
            None
        }
    }
}

/// Remove dead instructions from all blocks. Returns count removed.
fn remove_dead(function: &mut Function, used: &HashSet<VarId>) -> usize {
    let mut removed = 0;

    for block in &mut function.blocks {
        let before = block.instructions.len();
        block.instructions.retain(|inst| {
            if !is_removable(&inst.node) {
                return true; // side-effectful — keep
            }
            match get_dest(&inst.node) {
                Some(dest) => used.contains(&dest), // keep if used
                None => true,                       // no dest — keep
            }
        });
        removed += before - block.instructions.len();
    }

    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, BlockId, IntrinsicOp, Literal, Var};
    use crate::types::TypeSet;

    fn var(id: u32) -> VarId {
        VarId(id)
    }
    fn block(id: u32) -> BlockId {
        BlockId(id)
    }
    fn si(inst: Instruction) -> ast::Spanned<Instruction> {
        ast::Spanned::new(inst, ast::Span::default())
    }
    fn make_function(blocks: Vec<BasicBlock>, locals: Vec<Var>) -> Function {
        Function {
            name: ast::Identifier("test".into()),
            params: vec![],
            rest_param: None,
            blocks,
            locals,
            entry_block: BlockId(0),
        }
    }

    #[test]
    fn test_remove_unused_const() {
        // v0 = Const(42)  ← used by Return
        // v1 = Const(99)  ← unused → removed
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("y".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(99),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 1);
        assert_eq!(func.blocks[0].instructions.len(), 1);
    }

    #[test]
    fn test_cascading_removal() {
        // v0 = Const(1)   ← used only by v1
        // v1 = Add(v0, v0) ← unused → removed
        // After removing v1, v0 becomes unused → removed
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("y".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Intrinsic {
                    dest: var(1),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(0)],
                }),
            ],
            terminator: Terminator::Return { value: None },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 2);
        assert_eq!(func.blocks[0].instructions.len(), 0);
    }

    #[test]
    fn test_keep_used_instructions() {
        // v0 = Const(42)  ← used by Return
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("x".into()),
            TypeSet::uint(),
        )];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Const {
                dest: var(0),
                value: Literal::UInt(42),
            })],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 0);
        assert_eq!(func.blocks[0].instructions.len(), 1);
    }

    #[test]
    fn test_keep_impure_call() {
        // v0 = Call("log", [])  ← unused but Call is impure → keep
        let locals = vec![Var::new(
            var(0),
            ast::Identifier("r".into()),
            TypeSet::all(),
        )];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![si(Instruction::Call {
                dest: var(0),
                function: crate::ir::FunctionRef {
                    namespace: None,
                    name: ast::Identifier("log".into()),
                },
                args: vec![],
            })],
            terminator: Terminator::Return { value: None },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 0);
        assert_eq!(func.blocks[0].instructions.len(), 1);
    }

    #[test]
    fn test_keep_setindex() {
        // SetIndex has side effects — keep even though it has no dest
        let locals = vec![
            Var::new(
                var(0),
                ast::Identifier("arr".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
            Var::new(var(1), ast::Identifier("idx".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("val".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(0),
                }),
                si(Instruction::Const {
                    dest: var(2),
                    value: Literal::UInt(42),
                }),
                si(Instruction::SetIndex {
                    base: var(0),
                    key: var(1),
                    value: var(2),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 0);
        assert_eq!(func.blocks[0].instructions.len(), 3);
    }

    #[test]
    fn test_remove_unused_index() {
        // Index is pure — remove if result unused
        let locals = vec![
            Var::new(
                var(0),
                ast::Identifier("arr".into()),
                TypeSet::single(crate::types::BaseType::Array),
            ),
            Var::new(var(1), ast::Identifier("idx".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("elem".into()), TypeSet::all()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(0),
                }),
                si(Instruction::Index {
                    dest: var(2),
                    base: var(0),
                    key: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        // Index(v2) unused → removed, then Const(v1) only used by Index → removed
        assert_eq!(removed, 2);
        assert_eq!(func.blocks[0].instructions.len(), 0);
    }

    #[test]
    fn test_remove_unused_copy_after_propagation() {
        // Simulates what copy propagation leaves behind:
        // v0 = Const(42)
        // v1 = Copy(v0)   ← copy prop replaced all uses of v1 with v0
        // v2 = Add(v0, v0) ← was Add(v1, v1), now uses v0 directly
        // Return(v2)
        // → v1 is dead, remove it
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("y".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("r".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(42),
                }),
                si(Instruction::Copy {
                    dest: var(1),
                    src: var(0),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: IntrinsicOp::Add,
                    args: vec![var(0), var(0)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 1); // Copy removed
        assert_eq!(func.blocks[0].instructions.len(), 2); // Const + Add remain
    }

    #[test]
    fn test_keep_writeref() {
        // WriteRef has side effects — always keep
        let locals = vec![
            Var::new(var(0), ast::Identifier("r".into()), TypeSet::all()),
            Var::new(var(1), ast::Identifier("v".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(1),
                    value: Literal::UInt(99),
                }),
                si(Instruction::WriteRef {
                    ref_var: var(0),
                    value: var(1),
                }),
            ],
            terminator: Terminator::Return { value: None },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 0);
    }

    #[test]
    fn test_remove_unused_phi() {
        // Phi is pure — remove if dest unused
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("y".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("phi".into()), TypeSet::uint()),
        ];
        let blocks = vec![BasicBlock {
            id: block(0),
            instructions: vec![
                si(Instruction::Const {
                    dest: var(0),
                    value: Literal::UInt(1),
                }),
                si(Instruction::Phi {
                    dest: var(2),
                    sources: vec![(block(0), var(0)), (block(1), var(1))],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(0)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let removed = eliminate_dead_code(&mut func);

        assert_eq!(removed, 1); // Phi removed (unused)
        assert_eq!(func.blocks[0].instructions.len(), 1); // Const remains
    }
}
