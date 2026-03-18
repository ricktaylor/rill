//! Copy Propagation
//!
//! When a `Copy(dest, src)` instruction is the sole definition of `dest`,
//! replace all uses of `dest` with `src` and delete the Copy.
//!
//! This eliminates intermediate variables introduced by:
//! - Identity cast/widen elision (Cast → Copy)
//! - Read-only ref demotion (MakeRef → Copy)
//! - Phi resolution (Copy inserted into predecessors)
//!
//! Runs in the Phase 1 fixpoint loop. Feeds DCE: after propagation,
//! the Copy's dest has zero uses and can be removed.

use super::{Function, Instruction, Terminator, VarId};
use std::collections::HashMap;

/// Propagate copies: replace uses of `Copy(dest, src)` dest with src.
///
/// Returns the number of Copy instructions removed.
pub fn propagate_copies(function: &mut Function) -> usize {
    // Phase 1: collect Copy mappings (dest → src)
    let mut copy_map: HashMap<VarId, VarId> = HashMap::new();
    for block in &function.blocks {
        for inst in &block.instructions {
            if let Instruction::Copy { dest, src } = &inst.node {
                // Only propagate if dest != src (self-copy is a no-op)
                if dest != src {
                    copy_map.insert(*dest, *src);
                }
            }
        }
    }

    if copy_map.is_empty() {
        return 0;
    }

    // Resolve chains: if a → b and b → c, then a → c
    let resolved: HashMap<VarId, VarId> = copy_map
        .keys()
        .map(|&k| {
            let mut v = k;
            // Follow the chain with bounded iteration
            for _ in 0..64 {
                match copy_map.get(&v) {
                    Some(&next) => v = next,
                    None => break,
                }
            }
            (k, v)
        })
        .collect();

    // Phase 2: replace all uses of dest with resolved src
    let mut changes = 0;

    for block in &mut function.blocks {
        // Replace in instructions
        for inst in &mut block.instructions {
            let replaced = replace_vars_in_instruction(&mut inst.node, &resolved);
            changes += replaced;
        }

        // Replace in terminators
        changes += replace_vars_in_terminator(&mut block.terminator, &resolved);
    }

    // Phase 3: remove the Copy instructions themselves
    if changes > 0 {
        for block in &mut function.blocks {
            block.instructions.retain(|inst| {
                !matches!(&inst.node, Instruction::Copy { dest, .. } if resolved.contains_key(dest))
            });
        }
    }

    changes
}

/// Replace VarId references in an instruction. Returns count of replacements.
fn replace_vars_in_instruction(inst: &mut Instruction, map: &HashMap<VarId, VarId>) -> usize {
    let mut count = 0;

    match inst {
        // Don't replace the dest of a Copy that's being propagated
        Instruction::Copy { dest, src } => {
            if !map.contains_key(dest) {
                // This Copy is not being propagated — but its src might be
                if let Some(&new_src) = map.get(src) {
                    *src = new_src;
                    count += 1;
                }
            }
        }

        Instruction::Const { .. } | Instruction::Undefined { .. } => {}

        Instruction::Intrinsic { args, .. } => {
            for arg in args.iter_mut() {
                if let Some(&new) = map.get(arg) {
                    *arg = new;
                    count += 1;
                }
            }
        }

        Instruction::Index { base, key, .. } => {
            if let Some(&new) = map.get(base) {
                *base = new;
                count += 1;
            }
            if let Some(&new) = map.get(key) {
                *key = new;
                count += 1;
            }
        }

        Instruction::SetIndex { base, key, value } => {
            if let Some(&new) = map.get(base) {
                *base = new;
                count += 1;
            }
            if let Some(&new) = map.get(key) {
                *key = new;
                count += 1;
            }
            if let Some(&new) = map.get(value) {
                *value = new;
                count += 1;
            }
        }

        Instruction::Call { args, .. } => {
            for arg in args.iter_mut() {
                if let Some(&new) = map.get(&arg.value) {
                    arg.value = new;
                    count += 1;
                }
            }
        }

        Instruction::Phi { sources, .. } => {
            for (_, var) in sources.iter_mut() {
                if let Some(&new) = map.get(var) {
                    *var = new;
                    count += 1;
                }
            }
        }

        Instruction::MakeRef { base, key, .. } => {
            if let Some(&new) = map.get(base) {
                *base = new;
                count += 1;
            }
            if let Some(k) = key
                && let Some(&new) = map.get(k) {
                    *k = new;
                    count += 1;
                }
        }

        Instruction::WriteRef { ref_var, value } => {
            if let Some(&new) = map.get(ref_var) {
                *ref_var = new;
                count += 1;
            }
            if let Some(&new) = map.get(value) {
                *value = new;
                count += 1;
            }
        }

        Instruction::Drop { vars } => {
            for v in vars.iter_mut() {
                if let Some(&new) = map.get(v) {
                    *v = new;
                    count += 1;
                }
            }
        }
    }

    count
}

/// Replace VarId references in a terminator. Returns count of replacements.
fn replace_vars_in_terminator(term: &mut Terminator, map: &HashMap<VarId, VarId>) -> usize {
    let mut count = 0;

    match term {
        Terminator::If { condition, .. } => {
            if let Some(&new) = map.get(condition) {
                *condition = new;
                count += 1;
            }
        }
        Terminator::Match { value, .. } => {
            if let Some(&new) = map.get(value) {
                *value = new;
                count += 1;
            }
        }
        Terminator::Guard { value, .. } => {
            if let Some(&new) = map.get(value) {
                *value = new;
                count += 1;
            }
        }
        Terminator::Return { value: Some(v) } => {
            if let Some(&new) = map.get(v) {
                *v = new;
                count += 1;
            }
        }
        Terminator::Exit { value } => {
            if let Some(&new) = map.get(value) {
                *value = new;
                count += 1;
            }
        }
        Terminator::Jump { .. } | Terminator::Return { value: None } | Terminator::Unreachable => {}
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::ir::{BasicBlock, BlockId, Literal, Var};
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
    fn test_simple_copy_propagation() {
        // v0 = Const(42)
        // v1 = Copy(v0)
        // Return(v1)
        // → Return(v0), Copy removed
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
                si(Instruction::Copy {
                    dest: var(1),
                    src: var(0),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(1)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = propagate_copies(&mut func);

        assert!(changes > 0);
        // Return should now reference var(0) directly
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Return { value: Some(v) } if v == var(0)
        ));
        // Copy should be removed
        assert_eq!(func.blocks[0].instructions.len(), 1);
        assert!(matches!(
            func.blocks[0].instructions[0].node,
            Instruction::Const { .. }
        ));
    }

    #[test]
    fn test_chain_propagation() {
        // v0 = Const(42)
        // v1 = Copy(v0)
        // v2 = Copy(v1)
        // Return(v2)
        // → Return(v0), both Copies removed
        let locals = vec![
            Var::new(var(0), ast::Identifier("x".into()), TypeSet::uint()),
            Var::new(var(1), ast::Identifier("y".into()), TypeSet::uint()),
            Var::new(var(2), ast::Identifier("z".into()), TypeSet::uint()),
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
                si(Instruction::Copy {
                    dest: var(2),
                    src: var(1),
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = propagate_copies(&mut func);

        assert!(changes > 0);
        assert!(matches!(
            func.blocks[0].terminator,
            Terminator::Return { value: Some(v) } if v == var(0)
        ));
        // Both Copies removed
        assert_eq!(func.blocks[0].instructions.len(), 1);
    }

    #[test]
    fn test_propagation_into_intrinsic() {
        // v0 = Const(1)
        // v1 = Copy(v0)
        // v2 = Add(v1, v1)
        // → v2 = Add(v0, v0)
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
                    value: Literal::UInt(1),
                }),
                si(Instruction::Copy {
                    dest: var(1),
                    src: var(0),
                }),
                si(Instruction::Intrinsic {
                    dest: var(2),
                    op: crate::ir::IntrinsicOp::Add,
                    args: vec![var(1), var(1)],
                }),
            ],
            terminator: Terminator::Return {
                value: Some(var(2)),
            },
        }];

        let mut func = make_function(blocks, locals);
        let changes = propagate_copies(&mut func);

        assert!(changes > 0);
        // Add should now reference var(0) directly
        if let Instruction::Intrinsic { args, .. } = &func.blocks[0].instructions[1].node {
            assert_eq!(args[0], var(0));
            assert_eq!(args[1], var(0));
        } else {
            panic!("expected Intrinsic");
        }
    }

    #[test]
    fn test_no_propagation_without_copies() {
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
        let changes = propagate_copies(&mut func);

        assert_eq!(changes, 0);
    }
}
