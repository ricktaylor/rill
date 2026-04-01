//! mem2reg: Promote Assign/Read instructions to SSA form.
//!
//! Implements the Braun et al. (2013) algorithm for on-demand SSA construction.

use crate::ast;
use crate::ir::*;
use std::collections::HashMap;

/// Promote all `Assign`/`Read` instructions in a function to SSA form.
///
/// After this pass:
/// - All `Assign` instructions are removed
/// - All `Read` instructions are replaced with `Copy` (or eliminated)
/// - Phi nodes are inserted at control flow merge points
/// - The function is in proper SSA form
pub fn promote(function: &mut Function) {
    let mut ctx = PromoteCtx::new(function);
    ctx.run_on_blocks(&function.blocks);
    ctx.apply(function);
}

// ============================================================================
// Predecessor map
// ============================================================================

/// Build a map from each block to its predecessor blocks.
fn build_predecessors(function: &Function) -> HashMap<BlockId, Vec<BlockId>> {
    let mut preds: HashMap<BlockId, Vec<BlockId>> = HashMap::new();

    // Ensure every block has an entry (even if no predecessors)
    for block in &function.blocks {
        preds.entry(block.id).or_default();
    }

    for block in &function.blocks {
        for succ in block.terminator.successors() {
            preds.entry(succ).or_default().push(block.id);
        }
    }

    preds
}

// ============================================================================
// Promote context
// ============================================================================

struct PromoteCtx {
    /// Predecessor map: block → [predecessor blocks]
    predecessors: HashMap<BlockId, Vec<BlockId>>,

    /// Current definition of each variable in each block.
    /// `(variable_name, block_id) → VarId`
    current_def: HashMap<(ast::Identifier, BlockId), VarId>,

    /// Phi nodes to insert at the start of blocks.
    /// `block_id → [(dest_var, [(pred_block, pred_var)])]`
    inserted_phis: HashMap<BlockId, Vec<(VarId, Vec<(BlockId, VarId)>)>>,

    /// Replacement map: for each `Read` instruction's dest VarId,
    /// the resolved SSA VarId to use instead.
    read_replacements: HashMap<VarId, VarId>,

    /// Next VarId for fresh phi variables.
    next_var_id: u32,
}

impl PromoteCtx {
    fn new(function: &Function) -> Self {
        // Find the highest existing VarId so new ones don't collide
        let max_var = function.locals.iter().map(|v| v.id.0).max().unwrap_or(0);

        PromoteCtx {
            predecessors: build_predecessors(function),
            current_def: HashMap::new(),
            inserted_phis: HashMap::new(),
            read_replacements: HashMap::new(),
            next_var_id: max_var + 1,
        }
    }

    fn fresh_var(&mut self) -> VarId {
        let id = VarId(self.next_var_id);
        self.next_var_id += 1;
        id
    }

    // ── Braun et al. core ────────────────────────────────────────────

    /// Record a definition: variable `name` has value `value` in `block`.
    fn write_variable(&mut self, name: &ast::Identifier, block: BlockId, value: VarId) {
        self.current_def.insert((name.clone(), block), value);
    }

    /// Look up the current SSA value for `name` in `block`.
    ///
    /// If defined locally, returns it directly. Otherwise, recursively
    /// queries predecessors, inserting Phi nodes at merge points.
    fn read_variable(&mut self, name: &ast::Identifier, block: BlockId) -> VarId {
        if let Some(&val) = self.current_def.get(&(name.clone(), block)) {
            return val;
        }
        self.read_variable_recursive(name, block)
    }

    /// Recursive predecessor lookup (the core of Braun et al.).
    fn read_variable_recursive(&mut self, name: &ast::Identifier, block: BlockId) -> VarId {
        let preds = self.predecessors.get(&block).cloned().unwrap_or_default();

        let val = if preds.is_empty() {
            // Entry block with no predecessors — variable is undefined.
            // This happens for variables read before assignment (a semantic
            // error caught earlier by the lowerer). Use a fresh var that
            // will produce `undefined` at runtime.

            // Don't insert anything — the variable was never assigned.
            // The compiler will treat this as an undefined read.
            self.fresh_var()
        } else if preds.len() == 1 {
            // Single predecessor: inherit its definition directly.
            self.read_variable(name, preds[0])
        } else {
            // Multiple predecessors: insert a Phi node.
            //
            // Write a placeholder BEFORE recursing to break cycles (back-edges
            // in loops would otherwise cause infinite recursion).
            let phi_var = self.fresh_var();
            self.write_variable(name, block, phi_var);

            // Now resolve each predecessor's value.
            let sources: Vec<(BlockId, VarId)> = preds
                .iter()
                .map(|&pred| (pred, self.read_variable(name, pred)))
                .collect();

            // Try to simplify: if all sources are the same value (ignoring
            // self-references), the phi is trivial and can be eliminated.
            let simplified = try_remove_trivial_phi(phi_var, &sources);

            if simplified != phi_var {
                // Trivial: no phi needed, use the single reaching value.
                simplified
            } else {
                // Non-trivial: record the phi for insertion.
                self.inserted_phis
                    .entry(block)
                    .or_default()
                    .push((phi_var, sources));
                phi_var
            }
        };

        // Memoize so subsequent reads in the same block are O(1).
        self.write_variable(name, block, val);
        val
    }

    // ── Main pass ────────────────────────────────────────────────────

    /// Process all blocks from the function, resolving Assign/Read.
    ///
    /// Two-pass approach:
    /// 1. Record all Assigns (writeVariable) across all blocks first
    /// 2. Then resolve all Reads (readVariable) using the complete definition map
    ///
    /// This is necessary because readVariable recurses into predecessors.
    /// If a back-edge predecessor hasn't been processed yet, its Assign
    /// wouldn't be recorded, and the recursive lookup would miss it —
    /// making loop-carried phis look trivially self-referential.
    fn run_on_blocks(&mut self, blocks: &[BasicBlock]) {
        // Pass 1: record all definitions
        for block in blocks {
            for spanned_inst in &block.instructions {
                if let Instruction::Assign { name, value } = &spanned_inst.node {
                    self.write_variable(name, block.id, *value);
                }
            }
        }

        // Pass 2: resolve all reads (may insert phis via recursive lookup)
        for block in blocks {
            for spanned_inst in &block.instructions {
                if let Instruction::Read { name, dest } = &spanned_inst.node {
                    let resolved = self.read_variable(name, block.id);
                    self.read_replacements.insert(*dest, resolved);
                }
            }
        }
    }

    /// Apply the computed SSA form back to the function.
    fn apply(self, function: &mut Function) {
        // 1. Insert Phi nodes at the start of blocks that need them.
        for block in &mut function.blocks {
            if let Some(phis) = self.inserted_phis.get(&block.id) {
                let mut phi_instructions: Vec<SpannedInst> = phis
                    .iter()
                    .map(|(dest, sources)| {
                        ast::Spanned::new(
                            Instruction::Phi {
                                dest: *dest,
                                sources: sources.clone(),
                            },
                            ast::Span::default(),
                        )
                    })
                    .collect();
                // Prepend phis before existing instructions
                phi_instructions.append(&mut block.instructions);
                block.instructions = phi_instructions;
            }
        }

        // 2. Replace Read instructions with Copy, remove Assign instructions.
        for block in &mut function.blocks {
            block.instructions.retain_mut(|inst| {
                match &inst.node {
                    Instruction::Assign { .. } => false, // Remove
                    Instruction::Read { dest, .. } => {
                        // Replace with Copy to the resolved value
                        if let Some(&resolved) = self.read_replacements.get(dest) {
                            if resolved == *dest {
                                // Self-copy: remove entirely
                                false
                            } else {
                                inst.node = Instruction::Copy {
                                    dest: *dest,
                                    src: resolved,
                                };
                                true
                            }
                        } else {
                            // No resolution found — shouldn't happen in well-formed IR
                            false
                        }
                    }
                    _ => true, // Keep all other instructions
                }
            });
        }

        // 3. Register new Phi variables in the function's locals.
        for phis in self.inserted_phis.values() {
            for (dest, _) in phis {
                function.locals.push(Var::new(
                    *dest,
                    ast::Identifier("_phi".to_string()),
                    TypeSet::all(),
                ));
            }
        }
    }
}

// ============================================================================
// Trivial phi elimination
// ============================================================================

/// Check if a phi is trivial: all operands are the same value (ignoring
/// self-references to the phi itself).
///
/// Returns `phi_var` if non-trivial, or the single reaching value if trivial.
fn try_remove_trivial_phi(phi_var: VarId, sources: &[(BlockId, VarId)]) -> VarId {
    let mut same: Option<VarId> = None;
    for &(_, val) in sources {
        if val == phi_var {
            continue; // Self-reference: skip
        }
        match same {
            None => same = Some(val),
            Some(s) if s == val => continue, // Same as existing: skip
            Some(_) => return phi_var,       // Different values: non-trivial
        }
    }
    // If all operands were self-references, the phi is unreachable/undefined.
    // Otherwise, return the single value.
    same.unwrap_or(phi_var)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_block(id: u32, instructions: Vec<Instruction>, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            instructions: instructions
                .into_iter()
                .map(|inst| ast::Spanned::new(inst, ast::Span::default()))
                .collect(),
            terminator,
        }
    }

    fn make_function(blocks: Vec<BasicBlock>, locals: Vec<Var>) -> Function {
        Function {
            name: ast::Identifier("test".to_string()),
            params: Vec::new(),
            rest_param: None,
            locals,
            blocks,
            entry_block: BlockId(0),
        }
    }

    fn x() -> ast::Identifier {
        ast::Identifier("x".to_string())
    }

    /// Straight-line code: Assign then Read in the same block.
    /// Read should resolve to the assigned value — no phi needed.
    #[test]
    fn test_single_block_assign_read() {
        let mut func = make_function(
            vec![make_block(
                0,
                vec![
                    // x = v0
                    Instruction::Assign {
                        name: x(),
                        value: VarId(0),
                    },
                    // dest(v1) = read x
                    Instruction::Read {
                        name: x(),
                        dest: VarId(1),
                    },
                ],
                Terminator::Return {
                    value: Some(VarId(1)),
                },
            )],
            vec![
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::all()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::all()),
            ],
        );

        promote(&mut func);

        // The Read should be replaced with Copy { dest: v1, src: v0 }
        // The Assign should be removed
        assert_eq!(func.blocks[0].instructions.len(), 1);
        match &func.blocks[0].instructions[0].node {
            Instruction::Copy { dest, src } => {
                assert_eq!(*dest, VarId(1));
                assert_eq!(*src, VarId(0));
            }
            other => panic!("Expected Copy, got {other:?}"),
        }
    }

    /// Diamond CFG: assign different values in then/else branches,
    /// read after the merge. Should insert a Phi.
    #[test]
    fn test_diamond_phi() {
        // Block 0: branch to 1 or 2
        // Block 1: x = v1, jump to 3
        // Block 2: x = v2, jump to 3
        // Block 3: read x → should get Phi(v1, v2)
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![],
                    Terminator::If {
                        condition: VarId(0),
                        then_target: BlockId(1),
                        else_target: BlockId(2),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    1,
                    vec![Instruction::Assign {
                        name: x(),
                        value: VarId(1),
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(
                    2,
                    vec![Instruction::Assign {
                        name: x(),
                        value: VarId(2),
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(
                    3,
                    vec![Instruction::Read {
                        name: x(),
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("cond".into()), TypeSet::bool()),
                Var::new(VarId(1), ast::Identifier("v1".into()), TypeSet::all()),
                Var::new(VarId(2), ast::Identifier("v2".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("v3".into()), TypeSet::all()),
            ],
        );

        promote(&mut func);

        // Block 3 should now start with a Phi, followed by a Copy
        let b3 = &func.blocks[3];
        assert!(b3.instructions.len() >= 1);

        // First instruction should be a Phi
        match &b3.instructions[0].node {
            Instruction::Phi { sources, .. } => {
                assert_eq!(sources.len(), 2);
                // Sources should be from blocks 1 and 2
                let block_ids: Vec<BlockId> = sources.iter().map(|(b, _)| *b).collect();
                assert!(block_ids.contains(&BlockId(1)));
                assert!(block_ids.contains(&BlockId(2)));
            }
            other => panic!("Expected Phi, got {other:?}"),
        }
    }

    /// Loop: x is assigned before the loop, modified in the body,
    /// read after. Should insert a loop-carried Phi in the header.
    #[test]
    fn test_loop_phi() {
        // Block 0: x = v0, jump to 1 (header)
        // Block 1: read x, branch to 2 (body) or 3 (exit)
        // Block 2: x = v_new, jump to 1 (back-edge)
        // Block 3: read x
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![Instruction::Assign {
                        name: x(),
                        value: VarId(0),
                    }],
                    Terminator::Jump { target: BlockId(1) },
                ),
                make_block(
                    1,
                    vec![Instruction::Read {
                        name: x(),
                        dest: VarId(1),
                    }],
                    Terminator::If {
                        condition: VarId(10),
                        then_target: BlockId(2),
                        else_target: BlockId(3),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    2,
                    vec![Instruction::Assign {
                        name: x(),
                        value: VarId(2),
                    }],
                    Terminator::Jump { target: BlockId(1) },
                ),
                make_block(
                    3,
                    vec![Instruction::Read {
                        name: x(),
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("init".into()), TypeSet::all()),
                Var::new(VarId(1), ast::Identifier("r1".into()), TypeSet::all()),
                Var::new(VarId(2), ast::Identifier("body".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::all()),
                Var::new(VarId(10), ast::Identifier("cond".into()), TypeSet::bool()),
            ],
        );

        promote(&mut func);

        // Block 1 (header) should have a Phi with sources from block 0 and block 2
        let b1 = &func.blocks[1];
        let phi = b1
            .instructions
            .iter()
            .find(|i| matches!(i.node, Instruction::Phi { .. }));
        assert!(phi.is_some(), "Header block should have a Phi node");

        match &phi.unwrap().node {
            Instruction::Phi { sources, .. } => {
                let block_ids: Vec<BlockId> = sources.iter().map(|(b, _)| *b).collect();
                assert!(
                    block_ids.contains(&BlockId(0)),
                    "Phi should have entry edge"
                );
                assert!(block_ids.contains(&BlockId(2)), "Phi should have back-edge");
            }
            _ => unreachable!(),
        }
    }

    /// Trivial phi: both branches assign the same value.
    /// The phi should be eliminated.
    #[test]
    fn test_trivial_phi_elimination() {
        // Block 0: x = v0, branch to 1 or 2
        // Block 1: x = v0 (same!), jump to 3
        // Block 2: jump to 3 (x unchanged = v0)
        // Block 3: read x → should be v0 directly, no phi
        let mut func = make_function(
            vec![
                make_block(
                    0,
                    vec![Instruction::Assign {
                        name: x(),
                        value: VarId(0),
                    }],
                    Terminator::If {
                        condition: VarId(10),
                        then_target: BlockId(1),
                        else_target: BlockId(2),
                        span: ast::Span::default(),
                    },
                ),
                make_block(
                    1,
                    vec![Instruction::Assign {
                        name: x(),
                        value: VarId(0), // same value
                    }],
                    Terminator::Jump { target: BlockId(3) },
                ),
                make_block(2, vec![], Terminator::Jump { target: BlockId(3) }),
                make_block(
                    3,
                    vec![Instruction::Read {
                        name: x(),
                        dest: VarId(3),
                    }],
                    Terminator::Return {
                        value: Some(VarId(3)),
                    },
                ),
            ],
            vec![
                Var::new(VarId(0), ast::Identifier("v0".into()), TypeSet::all()),
                Var::new(VarId(3), ast::Identifier("r3".into()), TypeSet::all()),
                Var::new(VarId(10), ast::Identifier("cond".into()), TypeSet::bool()),
            ],
        );

        promote(&mut func);

        // Block 3 should have NO phi (trivial elimination)
        let b3 = &func.blocks[3];
        let has_phi = b3
            .instructions
            .iter()
            .any(|i| matches!(i.node, Instruction::Phi { .. }));
        assert!(!has_phi, "Trivial phi should be eliminated");

        // The Read should resolve directly to v0
        let copy = b3
            .instructions
            .iter()
            .find(|i| matches!(i.node, Instruction::Copy { .. }));
        match copy.map(|c| &c.node) {
            Some(Instruction::Copy { dest, src }) => {
                assert_eq!(*dest, VarId(3));
                assert_eq!(*src, VarId(0));
            }
            _ => panic!("Expected Copy to v0"),
        }
    }
}
