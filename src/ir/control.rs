//! Control Flow Lowering
//!
//! Handles if/while/loop/for/match expressions and pattern matching in conditions.

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Control Flow Lowering
    // ========================================================================

    /// Lower an if expression
    pub fn lower_if(
        &mut self,
        conditions: &[ast::IfCondition],
        then_block: &[ast::Stmt],
        then_expr: &Option<Box<ast::Expression>>,
        else_block: &Option<Vec<ast::Stmt>>,
        else_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        let else_bb = self.fresh_block();
        let join_bb = self.fresh_block();

        // Push scope for condition bindings (visible in then-block)
        self.push_scope();

        // Process each condition in the chain
        // Each condition either continues to the next or jumps to else
        for condition in conditions {
            match condition {
                ast::IfCondition::Bool(expr) => {
                    // Lower boolean expression and branch
                    let cond_var = self.lower_expression(expr);
                    let next_bb = self.fresh_block();
                    self.finish_block(Terminator::If {
                        condition: cond_var,
                        then_target: next_bb,
                        else_target: else_bb,
                        span: self.current_span,
                    });
                    self.current_block = next_bb;
                    self.current_instructions = Vec::new();
                }

                ast::IfCondition::Let { pattern, value } => {
                    // Lower value and check if pattern matches
                    let value_var = self.lower_expression(value);
                    self.lower_if_pattern(pattern, value_var, BindingMode::Value, else_bb, None);
                }

                ast::IfCondition::With { pattern, value } => {
                    // Lower value and check if pattern matches (by-reference)
                    let (value_var, ref_origin) = self.lower_ref_expression(value);
                    self.lower_if_pattern(
                        pattern,
                        value_var,
                        BindingMode::Reference,
                        else_bb,
                        ref_origin,
                    );
                }
            }
        }

        // All conditions passed - execute then-block
        for stmt in then_block {
            self.lower_stmt(stmt);
        }
        let then_value = if let Some(expr) = then_expr {
            self.lower_expression(expr)
        } else {
            let dest = self.new_temp(TypeSet::empty());
            self.emit(Instruction::Undefined { dest });
            dest
        };
        let then_exit_block = self.current_block;
        self.pop_scope(); // End condition bindings scope
        self.finish_block(Terminator::Jump { target: join_bb });

        // Else block
        self.current_block = else_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        if let Some(stmts) = else_block {
            for stmt in stmts {
                self.lower_stmt(stmt);
            }
        }
        let else_value = if let Some(expr) = else_expr {
            self.lower_expression(expr)
        } else {
            let dest = self.new_temp(TypeSet::empty());
            self.emit(Instruction::Undefined { dest });
            dest
        };
        let else_exit_block = self.current_block;
        self.pop_scope();
        self.finish_block(Terminator::Jump { target: join_bb });

        // Join block with phi
        self.current_block = join_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::all());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(then_exit_block, then_value), (else_exit_block, else_value)],
        });

        result
    }

    /// Lower a pattern match for if-let/if-with conditions
    /// On match: binds variables and continues to next instruction
    /// On mismatch: jumps to else_bb
    ///
    /// `ref_origin` is passed for `if with` bindings so that variable
    /// patterns record their ref origin for write-back via WriteRef.
    /// For compound patterns (Array, Map), element-level ref origins are
    /// created internally when mode is Reference.
    ///
    /// Optimization: Match terminators implicitly reject undefined values
    /// (they won't match any type pattern), so we only emit Guard when
    /// there's no subsequent Match (i.e., simple variable patterns).
    fn lower_if_pattern(
        &mut self,
        pattern: &ast::Pat,
        value: VarId,
        mode: BindingMode,
        else_bb: BlockId,
        ref_origin: Option<RefOrigin>,
    ) {
        match &pattern.node {
            ast::Pattern::Wildcard => {
                // Always matches, binds nothing
            }

            ast::Pattern::Variable(name) => {
                // Only presence check needed - no type constraint
                // Guard checks defined vs undefined
                self.emit_guard(value, else_bb);

                // Bind the variable
                match mode {
                    BindingMode::Value => {
                        let dest = self.new_var(name.clone(), TypeSet::from_types(all_types()));
                        self.emit(Instruction::Copy { dest, src: value });
                        self.bind(name, dest);
                    }
                    BindingMode::Reference => {
                        self.bind(name, value);
                        if let Some(origin) = ref_origin {
                            self.bind_ref(name, origin);
                        }
                    }
                }
            }

            ast::Pattern::Array(patterns) => {
                // Match checks type AND rejects undefined (no Guard needed)
                self.emit_match(value, MatchPattern::Array(patterns.len()), else_bb);

                // Bind each element
                for (i, elem_pat) in patterns.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });

                    let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::MakeRef {
                            dest,
                            base: value,
                            key: Some(idx),
                        });
                        let origin = RefOrigin { ref_var: dest };
                        (dest, Some(origin))
                    } else {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest,
                            base: value,
                            key: idx,
                        });
                        (dest, None)
                    };

                    self.lower_if_pattern(elem_pat, elem, mode, else_bb, elem_origin);
                }
            }

            ast::Pattern::Literal(lit) => {
                // Match checks value AND rejects undefined (no Guard needed)
                let lit_pattern = self.ast_literal_to_ir_literal(lit);
                self.emit_match(value, MatchPattern::Literal(lit_pattern), else_bb);
            }

            ast::Pattern::Type { type_name, binding } => {
                // Match checks type AND rejects undefined (no Guard needed)
                if let Some(base_type) = self.type_name_to_base_type(type_name) {
                    self.emit_match(value, MatchPattern::Type(base_type), else_bb);
                } else {
                    // Unknown type - always fail to else
                    self.finish_block(Terminator::Jump { target: else_bb });
                    let unreachable_bb = self.fresh_block();
                    self.current_block = unreachable_bb;
                    self.current_instructions = Vec::new();
                    return;
                }

                // If there's a nested binding, process it (pass ref_origin through)
                if let Some(inner_pat) = binding {
                    self.lower_if_pattern(inner_pat.as_ref(), value, mode, else_bb, ref_origin);
                }
            }

            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                // Match checks min length AND rejects undefined (no Guard needed)
                let min_len = before.len() + after.len();
                self.emit_match(value, MatchPattern::ArrayMin(min_len), else_bb);

                // Bind before elements
                for (i, pat) in before.iter().enumerate() {
                    let idx = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: idx,
                        value: Literal::UInt(i as u64),
                    });

                    let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::MakeRef {
                            dest,
                            base: value,
                            key: Some(idx),
                        });
                        let origin = RefOrigin { ref_var: dest };
                        (dest, Some(origin))
                    } else {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest,
                            base: value,
                            key: idx,
                        });
                        (dest, None)
                    };

                    self.lower_if_pattern(pat, elem, mode, else_bb, elem_origin);
                }

                // Compute length for rest and after patterns
                let length = self.emit_unary_intrinsic(IntrinsicOp::Len, value);

                // Bind rest variable as a zero-copy Sequence over the source array.
                // ArraySeq(array, start, end, mutable) -> Sequence(ArraySlice)
                // Mutability follows binding mode: with = mutable (write-back),
                // let = immutable (by-value iteration only).
                if let Some(rest_name) = rest {
                    let start = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: start,
                        value: Literal::UInt(before.len() as u64),
                    });

                    let after_len_val = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len_val,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let end = self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len_val);

                    let is_mutable = self.new_temp(TypeSet::single(types::BaseType::Bool));
                    self.emit(Instruction::Const {
                        dest: is_mutable,
                        value: Literal::Bool(matches!(mode, BindingMode::Reference)),
                    });

                    let rest_val = self.new_temp(TypeSet::single(types::BaseType::Sequence));
                    self.emit(Instruction::Intrinsic {
                        dest: rest_val,
                        op: IntrinsicOp::ArraySeq,
                        args: vec![value, start, end, is_mutable],
                    });

                    let rest_var = self.new_var(
                        rest_name.clone(),
                        TypeSet::single(types::BaseType::Sequence),
                    );
                    self.emit(Instruction::Copy {
                        dest: rest_var,
                        src: rest_val,
                    });
                    self.bind(rest_name, rest_var);
                }

                // Bind after elements (from end, using len - after.len() + i)
                if !after.is_empty() {
                    let after_len_val = self.new_temp(TypeSet::single(types::BaseType::UInt));
                    self.emit(Instruction::Const {
                        dest: after_len_val,
                        value: Literal::UInt(after.len() as u64),
                    });
                    let after_start =
                        self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len_val);

                    for (i, pat) in after.iter().enumerate() {
                        let offset = self.new_temp(TypeSet::single(types::BaseType::UInt));
                        self.emit(Instruction::Const {
                            dest: offset,
                            value: Literal::UInt(i as u64),
                        });
                        let idx = self.emit_binary_intrinsic(IntrinsicOp::Add, after_start, offset);

                        let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
                            let dest = self.new_temp(TypeSet::all());
                            self.emit(Instruction::MakeRef {
                                dest,
                                base: value,
                                key: Some(idx),
                            });
                            let origin = RefOrigin { ref_var: dest };
                            (dest, Some(origin))
                        } else {
                            let dest = self.new_temp(TypeSet::all());
                            self.emit(Instruction::Index {
                                dest,
                                base: value,
                                key: idx,
                            });
                            (dest, None)
                        };

                        self.lower_if_pattern(pat, elem, mode, else_bb, elem_origin);
                    }
                }
            }

            ast::Pattern::Map(entries) => {
                // Check it's a map, then destructure entries by key
                self.emit_match(value, MatchPattern::Type(types::BaseType::Map), else_bb);

                for (key_pat, val_pat) in entries {
                    let key_var = match &key_pat.node {
                        ast::Pattern::Literal(lit) => {
                            let lit_pattern = self.ast_literal_to_ir_literal(lit);
                            let k = self.new_temp(TypeSet::all());
                            self.emit(Instruction::Const {
                                dest: k,
                                value: lit_pattern,
                            });
                            k
                        }
                        ast::Pattern::Variable(name) => {
                            // Variable key: treat name as text key
                            let k = self.new_temp(TypeSet::single(types::BaseType::Text));
                            self.emit(Instruction::Const {
                                dest: k,
                                value: Literal::Text(name.to_string()),
                            });
                            k
                        }
                        _ => {
                            self.diagnostics.error(
                                diagnostics::DiagnosticCode::E105_InvalidPattern,
                                self.current_span,
                                "map pattern key must be a literal or identifier",
                            );
                            continue;
                        }
                    };

                    let (val, val_origin) = if matches!(mode, BindingMode::Reference) {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::MakeRef {
                            dest,
                            base: value,
                            key: Some(key_var),
                        });
                        let origin = RefOrigin { ref_var: dest };
                        (dest, Some(origin))
                    } else {
                        let dest = self.new_temp(TypeSet::all());
                        self.emit(Instruction::Index {
                            dest,
                            base: value,
                            key: key_var,
                        });
                        (dest, None)
                    };

                    // Value must be present for the pattern to match
                    self.emit_guard(val, else_bb);
                    self.lower_if_pattern(val_pat, val, mode, else_bb, val_origin);
                }
            }
        }
    }

    /// Emit Guard terminator: check value is defined
    /// On defined: continues in new block
    /// On undefined: jumps to fail_bb
    fn emit_guard(&mut self, value: VarId, fail_bb: BlockId) {
        let ok_bb = self.fresh_block();
        self.finish_block(Terminator::Guard {
            value,
            defined: ok_bb,
            undefined: fail_bb,
            span: self.current_span,
        });
        self.current_block = ok_bb;
        self.current_instructions = Vec::new();
    }

    /// Emit Match terminator: check value matches pattern
    /// Match implicitly rejects undefined (won't match any pattern)
    /// On match: continues in new block
    /// On no match: jumps to fail_bb
    fn emit_match(&mut self, value: VarId, pattern: MatchPattern, fail_bb: BlockId) {
        let ok_bb = self.fresh_block();
        self.finish_block(Terminator::Match {
            value,
            arms: vec![(pattern, ok_bb)],
            default: fail_bb,
            span: self.current_span,
        });
        self.current_block = ok_bb;
        self.current_instructions = Vec::new();
    }

    /// Convert AST literal to IR literal for pattern matching
    fn ast_literal_to_ir_literal(&mut self, lit: &ast::Literal) -> Literal {
        match lit {
            ast::Literal::Bool(b) => Literal::Bool(*b),
            ast::Literal::UInt(n) => Literal::UInt(*n),
            ast::Literal::Int(n) => Literal::Int(*n),
            ast::Literal::Float(f) => Literal::Float(*f),
            ast::Literal::Text(s) => Literal::Text(s.clone()),
            ast::Literal::Bytes(b) => Literal::Bytes(b.clone()),
            ast::Literal::Array(_) | ast::Literal::Map(_) => {
                self.diagnostics.error(
                    diagnostics::DiagnosticCode::E105_InvalidPattern,
                    self.current_span,
                    "array and map literals cannot be used in match patterns",
                );
                Literal::Bool(false) // fallback — error already emitted
            }
        }
    }

    /// Convert type name to BaseType
    /// Returns None and emits diagnostic for unknown types
    pub(crate) fn type_name_to_base_type(
        &mut self,
        name: &ast::Identifier,
    ) -> Option<types::BaseType> {
        match name.as_ref() {
            "Bool" => Some(types::BaseType::Bool),
            "UInt" => Some(types::BaseType::UInt),
            "Int" => Some(types::BaseType::Int),
            "Float" => Some(types::BaseType::Float),
            "Text" => Some(types::BaseType::Text),
            "Bytes" => Some(types::BaseType::Bytes),
            "Array" => Some(types::BaseType::Array),
            "Map" => Some(types::BaseType::Map),
            _ => {
                self.error_invalid_pattern(&format!("unknown type '{}'", name), self.current_span);
                None
            }
        }
    }

    /// Snapshot all variable bindings in the current scope stack.
    /// Returns (name, VarId) pairs for all visible variables.
    fn snapshot_scope(&self) -> Vec<(ast::Identifier, VarId)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::new();
        // Walk scopes from innermost to outermost (same as lookup order)
        for scope in self.scopes.iter().rev() {
            for (name, &var) in scope {
                if seen.insert(name.clone()) {
                    result.push((name.clone(), var));
                }
            }
        }
        result
    }

    /// Construct loop-carried phi nodes for a while/loop.
    ///
    /// Before the header: create a phi for each in-scope variable, bind the
    /// variable to the phi result. After the body: patch each phi with the
    /// post-body VarId. Variables not modified in the body get identity phis
    /// (same VarId for both sources) which the closure compiler eliminates.
    ///
    /// Returns the list of (phi_var, pre_loop_var, variable_name) for patching.
    fn create_loop_phis(
        &mut self,
        pre_header_bb: BlockId,
        scope_snapshot: &[(ast::Identifier, VarId)],
    ) -> Vec<(VarId, VarId, ast::Identifier)> {
        let mut phis = Vec::new();
        for (name, pre_loop_var) in scope_snapshot {
            let phi_var = self.new_temp(TypeSet::all());
            // Placeholder phi — body source added later
            self.emit(Instruction::Phi {
                dest: phi_var,
                sources: vec![(pre_header_bb, *pre_loop_var)],
            });
            // Rebind variable to phi result — header uses this VarId
            self.bind(name, phi_var);
            phis.push((phi_var, *pre_loop_var, name.clone()));
        }
        phis
    }

    /// Patch loop-carried phis after the body is lowered.
    /// Adds the post-body VarId as a second source for each phi.
    fn patch_loop_phis(
        &mut self,
        header_bb: BlockId,
        body_exit_bb: BlockId,
        phis: &[(VarId, VarId, ast::Identifier)],
    ) {
        // For each phi, find the current VarId for the variable (post-body)
        // and add it as a source from the body exit block
        let post_body_vars: Vec<(VarId, VarId)> = phis
            .iter()
            .map(|(phi_var, _pre_var, name)| {
                let post_var = self.lookup(name).unwrap_or(*phi_var);
                (*phi_var, post_var)
            })
            .collect();

        // Find the header block and patch each phi
        if let Some(header_block) = self.blocks.iter_mut().find(|b| b.id == header_bb) {
            for inst in &mut header_block.instructions {
                if let Instruction::Phi { dest, sources } = &mut inst.node {
                    for (phi_var, post_var) in &post_body_vars {
                        if *dest == *phi_var {
                            sources.push((body_exit_bb, *post_var));
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Lower a while loop
    pub fn lower_while(
        &mut self,
        condition: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        // Snapshot scope and jump to header
        let scope_snapshot = self.snapshot_scope();
        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: create loop-carried phis, then evaluate condition
        self.current_block = header_bb;
        self.current_instructions = Vec::new();
        let loop_phis = self.create_loop_phis(pre_header_bb, &scope_snapshot);

        let cond = self.lower_expression(condition);
        self.finish_block(Terminator::If {
            condition: cond,
            then_target: body_bb,
            else_target: exit_bb,
            span: self.current_span,
        });

        // Body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: header_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        let break_values = self.loop_stack.pop().unwrap().break_values;

        // Patch phis with post-body variable values
        let body_exit_bb = self.current_block;
        self.patch_loop_phis(header_bb, body_exit_bb, &loop_phis);

        self.pop_scope();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit block
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(if break_values.is_empty() {
            TypeSet::empty()
        } else {
            TypeSet::all()
        });
        if break_values.is_empty() {
            self.emit(Instruction::Undefined { dest: result });
        } else {
            self.emit(Instruction::Phi {
                dest: result,
                sources: break_values,
            });
        }
        result
    }

    /// Lower an infinite loop
    pub fn lower_loop(
        &mut self,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        // Snapshot scope and jump to body (which acts as the header for loop)
        let scope_snapshot = self.snapshot_scope();
        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: body_bb });

        // Body: create loop-carried phis, then execute body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        let loop_phis = self.create_loop_phis(pre_header_bb, &scope_snapshot);

        self.push_scope();

        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: body_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        let break_values = self.loop_stack.pop().unwrap().break_values;

        // Patch phis with post-body variable values
        let body_exit_bb = self.current_block;
        self.patch_loop_phis(body_bb, body_exit_bb, &loop_phis);

        self.pop_scope();

        self.finish_block(Terminator::Jump { target: body_bb });

        // Exit block (only reachable via break)
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::all());
        if break_values.is_empty() {
            self.emit(Instruction::Undefined { dest: result });
        } else {
            self.emit(Instruction::Phi {
                dest: result,
                sources: break_values,
            });
        }
        result
    }

    /// Lower a for loop using index-based iteration.
    ///
    /// Lowers `for x in iterable { body }` to:
    /// ```text
    /// iter = iterable
    /// length = core::len(iter)
    /// i = 0
    /// header: if core::lt(i, length) -> body, exit
    /// body:   elem = iter[i]; bind x = elem; ... body ...; i = core::add(i, 1); jump header
    /// exit:   undefined
    /// ```
    pub fn lower_for(
        &mut self,
        binding_is_value: bool,
        binding: &ast::ForBinding,
        iterable: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        // If the iterable is a Range expression, use the sequence path
        // (SeqNext-based consumption instead of index-based iteration).
        if matches!(iterable, ast::Expression::Range { .. }) {
            return self.lower_for_seq(binding_is_value, binding, iterable, body, body_expr);
        }

        let iter_var = self.lower_expression(iterable);

        // length = Len(iter)
        let length = self.emit_unary_intrinsic(IntrinsicOp::Len, iter_var);

        // i = 0
        let i_init = self.new_temp(TypeSet::single(types::BaseType::UInt));
        self.emit(Instruction::Const {
            dest: i_init,
            value: Literal::UInt(0),
        });

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let latch_bb = self.fresh_block(); // increment block (continue target)
        let exit_bb = self.fresh_block();

        // Snapshot scope for loop-carried phis (handles variables modified in body)
        let scope_snapshot = self.snapshot_scope();
        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: loop-carried phis + index phi, then check i < length
        self.current_block = header_bb;
        self.current_instructions = Vec::new();

        // Create phis for all in-scope variables (handles sum, etc.)
        let loop_phis = self.create_loop_phis(pre_header_bb, &scope_snapshot);

        // Manual phi for the loop counter (not in scope snapshot)
        let i_var = self.new_temp(TypeSet::single(types::BaseType::UInt));
        let i_phi_idx = self.current_instructions.len();
        self.emit(Instruction::Phi {
            dest: i_var,
            sources: vec![], // patched below
        });

        let has_more = self.emit_binary_intrinsic(IntrinsicOp::Lt, i_var, length);
        self.finish_block(Terminator::If {
            condition: has_more,
            then_target: body_bb,
            else_target: exit_bb,
            span: self.current_span,
        });

        // Body: index into iterable, bind variables, execute body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        let mode = if binding_is_value {
            BindingMode::Value
        } else {
            BindingMode::Reference
        };

        // Read element from iterable — use MakeRef for by-ref binding
        let (elem, elem_origin) = if matches!(mode, BindingMode::Reference) {
            let dest = self.new_temp(TypeSet::all());
            self.emit(Instruction::MakeRef {
                dest,
                base: iter_var,
                key: Some(i_var),
            });
            let origin = RefOrigin { ref_var: dest };
            (dest, Some(origin))
        } else {
            let dest = self.new_temp(TypeSet::all());
            self.emit(Instruction::Index {
                dest,
                base: iter_var,
                key: i_var,
            });
            (dest, None)
        };

        // Bind loop variable(s)
        match binding {
            ast::ForBinding::Single(name) => match mode {
                BindingMode::Value => {
                    let var = self.new_var(name.clone(), TypeSet::all());
                    self.emit(Instruction::Copy {
                        dest: var,
                        src: elem,
                    });
                    self.bind(name, var);
                }
                BindingMode::Reference => {
                    self.bind(name, elem);
                    if let Some(origin) = elem_origin.clone() {
                        self.bind_ref(name, origin);
                    }
                }
            },
            ast::ForBinding::Pair(key_name, val_name) => {
                let key_var = self.new_var(key_name.clone(), TypeSet::all());
                self.emit(Instruction::Copy {
                    dest: key_var,
                    src: i_var,
                });
                self.bind(key_name, key_var);

                match mode {
                    BindingMode::Value => {
                        let var = self.new_var(val_name.clone(), TypeSet::all());
                        self.emit(Instruction::Copy {
                            dest: var,
                            src: elem,
                        });
                        self.bind(val_name, var);
                    }
                    BindingMode::Reference => {
                        self.bind(val_name, elem);
                        if let Some(origin) = elem_origin.clone() {
                            self.bind_ref(val_name, origin);
                        }
                    }
                }
            }
        }

        // continue jumps to latch (increment), not header (avoids skipping i++)
        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: latch_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        self.loop_stack.pop();

        // Patch loop-carried phis for scope variables (sum, etc.)
        let body_exit_bb = self.current_block;
        self.patch_loop_phis(header_bb, body_exit_bb, &loop_phis);

        self.pop_scope();
        self.finish_block(Terminator::Jump { target: latch_bb });

        // Latch block: increment counter, jump back to header
        self.current_block = latch_bb;
        self.current_instructions = Vec::new();

        let one = self.new_temp(TypeSet::single(types::BaseType::UInt));
        self.emit(Instruction::Const {
            dest: one,
            value: Literal::UInt(1),
        });
        let i_next = self.emit_binary_intrinsic(IntrinsicOp::Add, i_var, one);

        // Also patch loop-carried phis from the latch block
        // (continue skips the body exit but still needs phi sources)
        self.patch_loop_phis(header_bb, latch_bb, &loop_phis);

        let latch_exit_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Patch the counter phi with sources from pre-header and latch
        let header_block = self
            .blocks
            .iter_mut()
            .find(|b| b.id == header_bb)
            .expect("for-loop header block must exist");
        let phi_inst = header_block
            .instructions
            .get_mut(i_phi_idx)
            .expect("for-loop phi instruction must exist at recorded index");
        match &mut phi_inst.node {
            Instruction::Phi { sources, .. } => {
                *sources = vec![(pre_header_bb, i_init), (latch_exit_bb, i_next)];
            }
            _ => panic!("for-loop instruction at phi index is not a Phi"),
        }

        // Exit
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest: result });
        result
    }

    /// Lower a for loop over a Sequence using SeqNext-based consumption.
    ///
    /// Lowers `for x in seq { body }` to:
    /// ```text
    /// seq = iterable
    /// header: elem = SeqNext(seq)
    ///         Guard elem → body, exit
    /// body:   bind x = elem; ... body ...; jump header
    /// exit:   undefined
    /// ```
    ///
    /// Unlike index-based iteration, this consumes the sequence in-place
    /// via SeqNext (mutates the SeqState). No counter or length needed.
    fn lower_for_seq(
        &mut self,
        binding_is_value: bool,
        binding: &ast::ForBinding,
        iterable: &ast::Expression,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expression>>,
    ) -> VarId {
        let seq_var = self.lower_expression(iterable);

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        // Snapshot scope for loop-carried phis
        let scope_snapshot = self.snapshot_scope();
        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: create loop-carried phis, then try SeqNext
        self.current_block = header_bb;
        self.current_instructions = Vec::new();
        let loop_phis = self.create_loop_phis(pre_header_bb, &scope_snapshot);

        // elem = SeqNext(seq) — returns next value or undefined if exhausted
        let elem = self.new_temp(TypeSet::all());
        self.emit(Instruction::Intrinsic {
            dest: elem,
            op: IntrinsicOp::SeqNext,
            args: vec![seq_var],
        });

        // Guard: if elem is defined → body, else → exit
        self.finish_block(Terminator::Guard {
            value: elem,
            defined: body_bb,
            undefined: exit_bb,
            span: self.current_span,
        });

        // Body: bind variables, execute body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        let _mode = if binding_is_value {
            BindingMode::Value
        } else {
            BindingMode::Reference
        };

        // Bind loop variable(s)
        // Note: sequences are always by-value (no backing collection to write back to).
        // The `mode` flag is still passed for consistency but MakeRef is not used.
        match binding {
            ast::ForBinding::Single(name) => {
                let var = self.new_var(name.clone(), TypeSet::all());
                self.emit(Instruction::Copy {
                    dest: var,
                    src: elem,
                });
                self.bind(name, var);
            }
            ast::ForBinding::Pair(_key_name, _val_name) => {
                // Pair binding on sequences doesn't have a natural key.
                // For ranges, the "key" would be the iteration count, but
                // that's not tracked in SeqNext. Bind both to the value for now.
                // TODO: track iteration index for pair binding on sequences
                let var = self.new_var(
                    match binding {
                        ast::ForBinding::Pair(_, val) => val.clone(),
                        _ => unreachable!(),
                    },
                    TypeSet::all(),
                );
                self.emit(Instruction::Copy {
                    dest: var,
                    src: elem,
                });
                if let ast::ForBinding::Pair(key_name, val_name) = binding {
                    self.bind(val_name, var);
                    // Key is undefined for sequences (no natural index)
                    let undef = self.new_temp(TypeSet::empty());
                    self.emit(Instruction::Undefined { dest: undef });
                    let key_var = self.new_var(key_name.clone(), TypeSet::all());
                    self.emit(Instruction::Copy {
                        dest: key_var,
                        src: undef,
                    });
                    self.bind(key_name, key_var);
                }
            }
        }

        // continue jumps back to header (try next element)
        self.loop_stack.push(LoopContext {
            break_target: exit_bb,
            continue_target: header_bb,
            break_values: Vec::new(),
        });

        for stmt in body {
            self.lower_stmt(stmt);
        }
        if let Some(expr) = body_expr {
            self.lower_expression(expr);
        }

        self.loop_stack.pop();

        // Patch loop-carried phis with post-body values
        let body_exit_bb = self.current_block;
        self.patch_loop_phis(header_bb, body_exit_bb, &loop_phis);

        self.pop_scope();
        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest: result });
        result
    }

    /// Lower a match expression
    ///
    /// Uses a linear chain of pattern checks. Each arm tries its pattern
    /// against the scrutinee; on mismatch, control falls through to the next arm.
    /// This reuses `lower_if_pattern` which already handles all pattern types.
    pub fn lower_match(&mut self, value: &ast::Expression, arms: &[ast::MatchArm]) -> VarId {
        let scrutinee = self.lower_expression(value);
        let exit_bb = self.fresh_block();

        let mut arm_results: Vec<(BlockId, VarId)> = Vec::new();

        for arm in arms {
            let next_bb = self.fresh_block();

            // Determine binding mode from the arm
            let mode = if arm.binding_is_value {
                BindingMode::Value
            } else {
                BindingMode::Reference
            };

            // Push scope for pattern bindings
            self.push_scope();

            // Check pattern — on mismatch, jumps to next_bb
            // No top-level ref_origin for match (the scrutinee is a value,
            // not an indexed access). Element-level ref origins are created
            // internally for Array/Map destructuring when mode is Reference.
            self.lower_if_pattern(&arm.pattern, scrutinee, mode, next_bb, None);

            // Check guard if present
            if let Some(ref guard) = arm.guard {
                let guard_val = self.lower_expression(guard);
                let guard_pass_bb = self.fresh_block();
                self.finish_block(Terminator::If {
                    condition: guard_val,
                    then_target: guard_pass_bb,
                    else_target: next_bb,
                    span: self.current_span,
                });

                self.current_block = guard_pass_bb;
                self.current_instructions = Vec::new();
            }

            // Execute arm body
            for stmt in &arm.body {
                self.lower_stmt(stmt);
            }
            let arm_value = if let Some(ref expr) = arm.body_expr {
                self.lower_expression(expr)
            } else {
                let dest = self.new_temp(TypeSet::empty());
                self.emit(Instruction::Undefined { dest });
                dest
            };

            arm_results.push((self.current_block, arm_value));
            self.pop_scope();
            self.finish_block(Terminator::Jump { target: exit_bb });

            // Continue to next arm on pattern mismatch
            self.current_block = next_bb;
            self.current_instructions = Vec::new();
        }

        // Final fallthrough (unreachable if patterns are exhaustive)
        let fallback = self.new_temp(TypeSet::empty());
        self.emit(Instruction::Undefined { dest: fallback });
        arm_results.push((self.current_block, fallback));
        self.finish_block(Terminator::Jump { target: exit_bb });

        // Exit block with phi
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        let result = self.new_temp(TypeSet::all());
        self.emit(Instruction::Phi {
            dest: result,
            sources: arm_results,
        });
        result
    }

    /// Lower a `..` / `..=` expression as a MakeSeq intrinsic.
    ///
    /// Creates a Sequence value (lazy, O(1) memory). The `inclusive`
    /// flag is passed as a Bool argument to handle both `..` and `..=`.
    pub fn lower_range(
        &mut self,
        start: &ast::Expression,
        end: &ast::Expression,
        inclusive: bool,
    ) -> VarId {
        let start_var = self.lower_expression(start);
        let end_var = self.lower_expression(end);
        let inclusive_var = self.new_temp(TypeSet::single(types::BaseType::Bool));
        self.emit(Instruction::Const {
            dest: inclusive_var,
            value: Literal::Bool(inclusive),
        });

        let dest = self.new_temp(TypeSet::single(types::BaseType::Sequence));
        self.emit(Instruction::Intrinsic {
            dest,
            op: IntrinsicOp::MakeSeq,
            args: vec![start_var, end_var, inclusive_var],
        });
        dest
    }
}
