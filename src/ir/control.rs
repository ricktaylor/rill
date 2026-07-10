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
        then_expr: &Option<Box<ast::Expr>>,
        else_block: &Option<Vec<ast::Stmt>>,
        else_expr: &Option<Box<ast::Expr>>,
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
            self.emit_undefined()
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
            self.emit_undefined()
        };
        let else_exit_block = self.current_block;
        self.pop_scope();
        self.finish_block(Terminator::Jump { target: join_bb });

        // Join block with phi for the if expression result
        self.current_block = join_bb;
        self.current_instructions = Vec::new();

        // Variable phis are handled by mem2reg — no manual merge needed.
        self.emit_phi(vec![
            (then_exit_block, then_value),
            (else_exit_block, else_value),
        ])
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
                let narrowed = self.emit_guard(value, else_bb);

                // Bind the variable (using narrowed value — Undefined excluded)
                match mode {
                    BindingMode::Value => {
                        self.bind(name, narrowed);
                    }
                    BindingMode::Reference => {
                        self.bind(name, narrowed);
                        if let Some(origin) = ref_origin {
                            self.bind_ref(name, origin);
                        }
                    }
                }
            }

            ast::Pattern::Array(patterns) => {
                // Match checks type AND rejects undefined (no Guard needed)
                let value = self.emit_match(value, MatchPattern::Array(patterns.len()), else_bb);
                let base_name = Self::element_base_name(ref_origin.as_ref());

                // Bind each element
                for (i, elem_pat) in patterns.iter().enumerate() {
                    let idx = self.emit_const(Literal::UInt(i as u64));
                    let (elem, elem_origin) =
                        self.bind_element(value, idx, mode, base_name.clone(), TypeSet::any());
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
                let narrowed = if let Some(base_type) = self.type_name_to_base_type(type_name) {
                    self.emit_match(value, MatchPattern::Type(base_type), else_bb)
                } else {
                    // Unknown type - always fail to else
                    self.finish_block(Terminator::Jump { target: else_bb });
                    let unreachable_bb = self.fresh_block();
                    self.current_block = unreachable_bb;
                    self.current_instructions = Vec::new();
                    return;
                };

                // If there's a nested binding, process it with narrowed value
                if let Some(inner_pat) = binding {
                    self.lower_if_pattern(inner_pat.as_ref(), narrowed, mode, else_bb, ref_origin);
                }
            }

            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                // Match checks min length AND rejects undefined (no Guard needed)
                let min_len = before.len() + after.len();
                let value = self.emit_match(value, MatchPattern::ArrayMin(min_len), else_bb);
                let base_name = Self::element_base_name(ref_origin.as_ref());

                // Bind before elements
                for (i, pat) in before.iter().enumerate() {
                    let idx = self.emit_const(Literal::UInt(i as u64));
                    let (elem, elem_origin) =
                        self.bind_element(value, idx, mode, base_name.clone(), TypeSet::any());
                    self.lower_if_pattern(pat, elem, mode, else_bb, elem_origin);
                }

                // Compute length for rest and after patterns
                let length = self.emit_unary_intrinsic(IntrinsicOp::Len, value);

                // Bind rest variable as a zero-copy Sequence over the source array.
                // ArraySeq(mode, [array, start, end]) -> Sequence(ArraySlice)
                // SliceMode follows binding mode: with = Mutable (write-back),
                // let = ReadOnly (by-value iteration only).
                // A start < end guard produces undefined for empty slices.
                if let Some(rest_name) = rest {
                    let start = self.emit_const(Literal::UInt(before.len() as u64));

                    let after_len_val = self.emit_const(Literal::UInt(after.len() as u64));
                    let end = self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len_val);

                    let slice_mode = if matches!(mode, BindingMode::Reference) {
                        types::SliceMode::Mutable
                    } else {
                        types::SliceMode::ReadOnly
                    };

                    // Guard: start < end — empty slices produce undefined
                    let valid = self.emit_binary_intrinsic(IntrinsicOp::Lt, start, end);
                    let seq_bb = self.fresh_block();
                    let undef_bb = self.fresh_block();
                    let join_bb = self.fresh_block();
                    self.finish_block(Terminator::If {
                        condition: valid,
                        then_target: seq_bb,
                        else_target: undef_bb,
                        span: self.current_span,
                    });

                    // Then: create the slice sequence
                    self.current_block = seq_bb;
                    self.current_instructions = Vec::new();
                    let seq_val = self.new_temp(TypeSet::sequence());
                    self.emit(Instruction::Intrinsic {
                        dest: seq_val,
                        op: IntrinsicOp::ArraySeq(slice_mode),
                        args: vec![value, start, end],
                    });
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // Else: undefined
                    self.current_block = undef_bb;
                    self.current_instructions = Vec::new();
                    let undef_val = self.emit_undefined();
                    self.finish_block(Terminator::Jump { target: join_bb });

                    // Join: phi
                    self.current_block = join_bb;
                    self.current_instructions = Vec::new();
                    let rest_val = self.emit_phi(vec![(seq_bb, seq_val), (undef_bb, undef_val)]);

                    self.bind(rest_name, rest_val);
                }

                // Bind after elements (from end, using len - after.len() + i)
                if !after.is_empty() {
                    let after_len_val = self.emit_const(Literal::UInt(after.len() as u64));
                    let after_start =
                        self.emit_binary_intrinsic(IntrinsicOp::Sub, length, after_len_val);

                    for (i, pat) in after.iter().enumerate() {
                        let offset = self.emit_const(Literal::UInt(i as u64));
                        let idx = self.emit_binary_intrinsic(IntrinsicOp::Add, after_start, offset);
                        let (elem, elem_origin) =
                            self.bind_element(value, idx, mode, base_name.clone(), TypeSet::any());
                        self.lower_if_pattern(pat, elem, mode, else_bb, elem_origin);
                    }
                }
            }

            ast::Pattern::Map(entries) => {
                // Check it's a map, then destructure entries by key
                let value =
                    self.emit_match(value, MatchPattern::Type(types::BaseType::Map), else_bb);

                for (key_pat, val_pat) in entries {
                    let key_var = match &key_pat.node {
                        ast::Pattern::Literal(lit) => {
                            let lit_pattern = self.ast_literal_to_ir_literal(lit);
                            self.emit_const(lit_pattern)
                        }
                        ast::Pattern::Variable(name) => {
                            // Variable key: treat name as text key
                            self.emit_const(Literal::Text(name.to_string()))
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

                    let (val, val_origin) = self.bind_element(
                        value,
                        key_var,
                        mode,
                        Self::element_base_name(ref_origin.as_ref()),
                        TypeSet::any(),
                    );

                    // Value must be present for the pattern to match
                    let narrowed_val = self.emit_guard(val, else_bb);
                    self.lower_if_pattern(val_pat, narrowed_val, mode, else_bb, val_origin);
                }
            }
        }
    }

    /// Emit Guard terminator: check value is defined
    /// On defined: continues in new block
    /// On undefined: jumps to fail_bb
    /// Emit a definedness guard (Match with Undefined arm).
    ///
    /// On defined: continues in new block with a narrowed VarId (Undefined excluded)
    /// On undefined: jumps to fail_bb
    ///
    /// Returns the narrowed VarId for use in the match arm. The narrowed var
    /// has the same type as the original but with Undefined excluded.
    pub fn emit_guard(&mut self, value: VarId, fail_bb: BlockId) -> VarId {
        let ok_bb = self.fresh_block();
        self.finish_block(Terminator::Match {
            value,
            arms: vec![(MatchPattern::Type(types::BaseType::Undefined), fail_bb)],
            default: ok_bb,
            span: self.current_span,
        });
        self.current_block = ok_bb;
        self.current_instructions = Vec::new();

        // Narrowing copy: value is provably defined on this path
        self.emit_narrowing(
            value,
            self.var_type(value).difference(&TypeSet::undefined()),
        )
    }

    /// Emit Match terminator: check value matches pattern.
    ///
    /// On match: continues in new block with a narrowed VarId
    /// On no match: jumps to fail_bb
    ///
    /// Returns the narrowed VarId for use in the match arm. The narrowed var
    /// has the type implied by the pattern.
    fn emit_match(&mut self, value: VarId, pattern: MatchPattern, fail_bb: BlockId) -> VarId {
        let narrowed_type = match &pattern {
            MatchPattern::Type(ty) => TypeSet::single(*ty),
            MatchPattern::Literal(lit) => TypeSet::single(lit.base_type()),
            MatchPattern::Array(_) | MatchPattern::ArrayMin(_) => {
                TypeSet::single(types::BaseType::Array)
            }
        };

        let ok_bb = self.fresh_block();
        self.finish_block(Terminator::Match {
            value,
            arms: vec![(pattern, ok_bb)],
            default: fail_bb,
            span: self.current_span,
        });
        self.current_block = ok_bb;
        self.current_instructions = Vec::new();

        // Narrowing copy: value's type is narrowed to the pattern's type
        self.emit_narrowing(value, self.var_type(value).intersection(&narrowed_type))
    }

    /// Emit a narrowing copy if the type actually narrows.
    /// Returns the narrowed VarId, or the original if no narrowing needed.
    pub fn emit_narrowing(&mut self, src: VarId, narrowed: TypeSet) -> VarId {
        let src_type = self.var_type(src);
        if narrowed != src_type && !narrowed.is_empty() {
            self.emit_copy(src, narrowed)
        } else {
            src
        }
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
    pub fn type_name_to_base_type(&mut self, name: &ast::Identifier) -> Option<types::BaseType> {
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

    /// Lower a while loop
    pub fn lower_while(
        &mut self,
        condition: &ast::Expr,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expr>>,
    ) -> VarId {
        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: evaluate condition
        self.current_block = header_bb;
        self.current_instructions = Vec::new();

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

        self.pop_scope();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit block
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        if break_values.is_empty() {
            self.emit_undefined()
        } else {
            self.emit_phi(break_values)
        }
    }

    /// Lower an infinite loop
    pub fn lower_loop(&mut self, body: &[ast::Stmt], body_expr: &Option<Box<ast::Expr>>) -> VarId {
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        self.finish_block(Terminator::Jump { target: body_bb });

        // Body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();

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

        self.pop_scope();

        self.finish_block(Terminator::Jump { target: body_bb });

        // Exit block (only reachable via break)
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        if break_values.is_empty() {
            self.emit_undefined()
        } else {
            self.emit_phi(break_values)
        }
    }

    /// Lower a for loop with type dispatch.
    ///
    /// Evaluates the iterable, then dispatches on its runtime type:
    /// - Sequence → SeqNext-based consumption (no counter, Guard on exhaustion)
    /// - Default (Array, Map, etc.) → index-based iteration (Len + Lt + Index)
    ///
    /// Both paths lower the body independently (separate SSA variables).
    /// Variable-binding phis are handled by the mem2reg pass.
    /// When the iterable type is known at compile time, the optimizer
    /// collapses the Match to a single path.
    pub fn lower_for(
        &mut self,
        binding_is_value: bool,
        binding: &ast::ForBinding,
        iterable: &ast::Expr,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expr>>,
    ) -> VarId {
        let iter_var = self.lower_expression(iterable);

        let seq_bb = self.fresh_block();
        let map_bb = self.fresh_block();
        let idx_bb = self.fresh_block();
        let join_bb = self.fresh_block();

        // Type dispatch is compiler-internal — use default span to suppress
        // unreachable arm warnings (the user didn't write this match).
        // Maps iterate their entries (real keys + values) on a dedicated path;
        // Array/Text/Bytes index positionally on the default path.
        self.finish_block(Terminator::Match {
            value: iter_var,
            arms: vec![
                (MatchPattern::Type(types::BaseType::Sequence), seq_bb),
                (MatchPattern::Type(types::BaseType::Map), map_bb),
            ],
            default: idx_bb,
            span: ast::Span::default(),
        });

        // === Sequence path ===
        self.current_block = seq_bb;
        self.current_instructions = Vec::new();
        self.lower_for_seq(iter_var, binding_is_value, binding, body, body_expr);
        self.finish_block(Terminator::Jump { target: join_bb });

        // === Map path ===
        self.current_block = map_bb;
        self.current_instructions = Vec::new();
        // Narrow to Map so Len/MapKeyAt/Index see a known map base.
        let map_type = self.var_type(iter_var);
        let iter_map = self.emit_narrowing(iter_var, map_type.intersection(&TypeSet::map()));
        self.lower_for_map(iter_map, binding_is_value, binding, body, body_expr);
        self.finish_block(Terminator::Jump { target: join_bb });

        // === Index path (Array, Text, Bytes) ===
        self.current_block = idx_bb;
        self.current_instructions = Vec::new();
        // Narrow iter_var to exclude Sequence and Map — the Match dispatch
        // guarantees only positionally-indexable values reach this path. Without
        // this narrowing copy, the SSA type would still include them, causing
        // type guards on Len to reject valid values.
        let iter_type = self.var_type(iter_var);
        let iter_narrowed = self.emit_narrowing(
            iter_var,
            iter_type
                .difference(&TypeSet::sequence())
                .difference(&TypeSet::map()),
        );
        self.lower_for_idx(iter_narrowed, binding_is_value, binding, body, body_expr);
        self.finish_block(Terminator::Jump { target: join_bb });

        // === Join ===
        self.current_block = join_bb;
        self.current_instructions = Vec::new();

        self.emit_undefined()
    }

    /// Lower the index-based iteration path (for Array, Text, Bytes).
    /// Maps are handled by `lower_for_map` (entries, not positional index).
    ///
    /// ```text
    /// length = Len(iter)
    /// i = 0
    /// header: if Lt(i, length) → body, exit
    /// body:   elem = iter[i]; bind x; ... body ...; jump latch
    /// latch:  i = i + 1; jump header
    /// exit:
    /// ```
    ///
    /// After this returns, `self.current_block` is the exit block.
    fn lower_for_idx(
        &mut self,
        iter_var: VarId,
        binding_is_value: bool,
        binding: &ast::ForBinding,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expr>>,
    ) {
        // Use default span for compiler-generated loop mechanics (Len, Lt, Add).
        // These are internal control flow — diagnostics about them are noise.
        // The user's span is restored for the loop body.
        let user_span = self.current_span;
        self.current_span = ast::Span::default();

        // length = Len(iter) — guarded so the optimizer sees the collection type.
        // iter_var has been narrowed to exclude Sequence by the caller (lower_for).
        let length = self.lower_guarded_expression(|s: &mut Self| {
            s.emit_unary_intrinsic(IntrinsicOp::Len, iter_var)
        });

        // i = 0
        let i_init = self.emit_const(Literal::UInt(0));

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let latch_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: index phi, then check i < length
        self.current_block = header_bb;
        self.current_instructions = Vec::new();

        let i_var = self.new_temp(TypeSet::uint());
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
            span: ast::Span::default(),
        });

        // Restore user span for the loop body
        self.current_span = user_span;

        // Body
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        let mode = if binding_is_value {
            BindingMode::Value
        } else {
            BindingMode::Reference
        };

        // Index is bounded by i < len(iter_var) — element is always defined.
        let (elem, elem_origin) =
            self.bind_element(iter_var, i_var, mode, None, TypeSet::defined());

        match binding {
            ast::ForBinding::Single(name) => match mode {
                BindingMode::Value => {
                    self.bind(name, elem);
                }
                BindingMode::Reference => {
                    self.bind(name, elem);
                    if let Some(origin) = elem_origin.clone() {
                        self.bind_ref(name, origin);
                    }
                }
            },
            ast::ForBinding::Pair(key_name, val_name) => {
                self.bind(key_name, i_var);

                match mode {
                    BindingMode::Value => {
                        self.bind(val_name, elem);
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

        self.pop_scope();
        self.finish_block(Terminator::Jump { target: latch_bb });

        // Latch: increment counter (compiler-generated — default span)
        self.current_block = latch_bb;
        self.current_instructions = Vec::new();
        self.current_span = ast::Span::default();

        let one = self.emit_const(Literal::UInt(1));
        let i_next = self.emit_binary_intrinsic(IntrinsicOp::Add, i_var, one);

        let latch_exit_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Patch counter phi
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

        // Exit — leave current_block set to exit_bb for the dispatcher
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();
    }

    /// Lower map iteration (`for k, v in map` / `for x in map`).
    ///
    /// Mirrors `lower_for_idx`'s counter skeleton, but the loop body extracts the
    /// real entry via `MapKeyAt(map, i)` (the i-th key, `IndexMap` order) rather
    /// than using the counter as the key: `k` binds to the real key (always
    /// by-value), `v`/`x` to `map[real_key]` (the value; by-ref enables
    /// write-back to `map[real_key]`).
    fn lower_for_map(
        &mut self,
        iter_var: VarId,
        binding_is_value: bool,
        binding: &ast::ForBinding,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expr>>,
    ) {
        let user_span = self.current_span;
        self.current_span = ast::Span::default();

        // length = Len(map) — guarded so the optimizer sees the Map type.
        let length = self.lower_guarded_expression(|s: &mut Self| {
            s.emit_unary_intrinsic(IntrinsicOp::Len, iter_var)
        });

        let i_init = self.emit_const(Literal::UInt(0));

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let latch_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        let pre_header_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: index phi, then check i < length
        self.current_block = header_bb;
        self.current_instructions = Vec::new();

        let i_var = self.new_temp(TypeSet::uint());
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
            span: ast::Span::default(),
        });

        // Body
        self.current_span = user_span;
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        self.push_scope();

        let mode = if binding_is_value {
            BindingMode::Value
        } else {
            BindingMode::Reference
        };

        // The real key at position i (i < len ⇒ the entry exists, so both the
        // key and `map[key]` are defined).
        let real_key = self.new_temp(TypeSet::defined());
        self.emit(Instruction::Intrinsic {
            dest: real_key,
            op: IntrinsicOp::MapKeyAt,
            args: vec![iter_var, i_var],
        });

        // Value = map[real_key]; by-ref binds an accessor for write-back.
        let (elem, elem_origin) =
            self.bind_element(iter_var, real_key, mode, None, TypeSet::defined());

        match binding {
            // Single binding over a map yields the value.
            ast::ForBinding::Single(name) => match mode {
                BindingMode::Value => {
                    self.bind(name, elem);
                }
                BindingMode::Reference => {
                    self.bind(name, elem);
                    if let Some(origin) = elem_origin.clone() {
                        self.bind_ref(name, origin);
                    }
                }
            },
            // Pair binding: key (always by-value) + value.
            ast::ForBinding::Pair(key_name, val_name) => {
                self.bind(key_name, real_key);
                match mode {
                    BindingMode::Value => {
                        self.bind(val_name, elem);
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

        self.pop_scope();
        self.finish_block(Terminator::Jump { target: latch_bb });

        // Latch: increment counter
        self.current_block = latch_bb;
        self.current_instructions = Vec::new();
        self.current_span = ast::Span::default();

        let one = self.emit_const(Literal::UInt(1));
        let i_next = self.emit_binary_intrinsic(IntrinsicOp::Add, i_var, one);

        let latch_exit_bb = self.current_block;
        self.finish_block(Terminator::Jump { target: header_bb });

        // Patch counter phi
        let header_block = self
            .blocks
            .iter_mut()
            .find(|b| b.id == header_bb)
            .expect("for-map header block must exist");
        let phi_inst = header_block
            .instructions
            .get_mut(i_phi_idx)
            .expect("for-map phi instruction must exist at recorded index");
        match &mut phi_inst.node {
            Instruction::Phi { sources, .. } => {
                *sources = vec![(pre_header_bb, i_init), (latch_exit_bb, i_next)];
            }
            _ => panic!("for-map instruction at phi index is not a Phi"),
        }

        // Exit — leave current_block set to exit_bb for the dispatcher
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();
    }

    /// Lower the SeqNext-based iteration path (for Sequence types).
    ///
    /// ```text
    /// header: elem = SeqNext(seq)
    ///         Guard elem → body, exit
    /// body:   bind x = elem; ... body ...; jump header
    /// exit:
    /// ```
    ///
    /// After this returns, `self.current_block` is the exit block.
    fn lower_for_seq(
        &mut self,
        seq_var: VarId,
        _binding_is_value: bool,
        binding: &ast::ForBinding,
        body: &[ast::Stmt],
        body_expr: &Option<Box<ast::Expr>>,
    ) {
        // Use default span for compiler-generated loop mechanics (SeqNext).
        // Restore user span for the loop body.
        let user_span = self.current_span;
        self.current_span = ast::Span::default();

        let header_bb = self.fresh_block();
        let body_bb = self.fresh_block();
        let exit_bb = self.fresh_block();

        self.finish_block(Terminator::Jump { target: header_bb });

        // Header: SeqNext + Guard
        self.current_block = header_bb;
        self.current_instructions = Vec::new();

        let elem = self.emit_unary_intrinsic(IntrinsicOp::SeqNext, seq_var);

        // SeqNext exhaustion guard is compiler-internal — use default span
        // to suppress spurious diagnostics.
        self.finish_block(Terminator::Match {
            value: elem,
            arms: vec![(MatchPattern::Type(types::BaseType::Undefined), exit_bb)],
            default: body_bb,
            span: ast::Span::default(),
        });

        // Restore user span for the loop body
        self.current_span = user_span;

        // Body — elem is provably defined (Undefined took exit_bb)
        self.current_block = body_bb;
        self.current_instructions = Vec::new();
        let elem = self.emit_narrowing(elem, TypeSet::defined());
        self.push_scope();

        // Sequences are always by-value (no backing collection to write back to).
        match binding {
            ast::ForBinding::Single(name) => {
                self.bind(name, elem);
            }
            ast::ForBinding::Pair(key_name, val_name) => {
                self.bind(val_name, elem);
                // Key is undefined for sequences (no natural index)
                let undef = self.emit_undefined();
                self.bind(key_name, undef);
            }
        }

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

        self.pop_scope();
        self.finish_block(Terminator::Jump { target: header_bb });

        // Exit — leave current_block set to exit_bb for the dispatcher
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();
    }

    /// Lower a match expression
    ///
    /// Uses a linear chain of pattern checks. Each arm tries its pattern
    /// against the scrutinee; on mismatch, control falls through to the next arm.
    /// This reuses `lower_if_pattern` which already handles all pattern types.
    pub fn lower_match(&mut self, value: &ast::Expr, arms: &[ast::MatchArm]) -> VarId {
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
                self.emit_undefined()
            };

            let exit_block = self.current_block;
            arm_results.push((exit_block, arm_value));
            self.pop_scope();
            self.finish_block(Terminator::Jump { target: exit_bb });

            // Continue to next arm on pattern mismatch
            self.current_block = next_bb;
            self.current_instructions = Vec::new();
        }

        // Final fallthrough (unreachable if patterns are exhaustive)
        let fallback = self.emit_undefined();
        let fallback_block = self.current_block;
        arm_results.push((fallback_block, fallback));
        self.finish_block(Terminator::Jump { target: exit_bb });

        // Exit block with phi for the match result
        self.current_block = exit_bb;
        self.current_instructions = Vec::new();

        self.emit_phi(arm_results)
    }

    /// Lower a `..` / `..=` expression as a MakeSeq intrinsic.
    ///
    /// Creates a Sequence value (lazy, O(1) memory). MakeSeq always takes
    /// an exclusive end. For inclusive ranges (`..=`), the lowerer emits
    /// `end + 1` using checked Add — overflow on `0..=u64::MAX` produces
    /// undefined naturally.
    ///
    /// A `start < end` guard is emitted so that reversed ranges produce
    /// undefined, and the optimizer can prove the range is non-empty when
    /// execution reaches the loop body.
    pub fn lower_range(&mut self, start: &ast::Expr, end: &ast::Expr, inclusive: bool) -> VarId {
        self.lower_guarded_expression(|s: &mut Self| {
            let start_var = s.lower_expression(start);
            let end_var = s.lower_expression(end);

            // For inclusive ranges, emit end_excl = end + 1 (checked)
            let end_excl = if inclusive {
                let one = s.emit_const(Literal::UInt(1));
                s.emit_binary_intrinsic(IntrinsicOp::Add, end_var, one)
            } else {
                end_var
            };

            // Guard: start < end_excl — reversed ranges produce undefined
            let valid = s.emit_binary_intrinsic(IntrinsicOp::Lt, start_var, end_excl);

            let seq_bb = s.fresh_block();
            let undef_bb = s.fresh_block();
            let join_bb = s.fresh_block();

            s.finish_block(Terminator::If {
                condition: valid,
                then_target: seq_bb,
                else_target: undef_bb,
                span: s.current_span,
            });

            // Then: create the sequence
            s.current_block = seq_bb;
            s.current_instructions = Vec::new();
            let seq_val = s.emit_binary_intrinsic(IntrinsicOp::MakeSeq, start_var, end_excl);
            // Capture current_block AFTER emit_binary_intrinsic — the guard may
            // have created new blocks, shifting current_block away from seq_bb.
            let seq_exit = s.current_block;
            s.finish_block(Terminator::Jump { target: join_bb });

            // Else: undefined
            s.current_block = undef_bb;
            s.current_instructions = Vec::new();
            let undef_val = s.emit_undefined();
            s.finish_block(Terminator::Jump { target: join_bb });

            // Join: phi — use seq_exit (not seq_bb) as predecessor
            s.current_block = join_bb;
            s.current_instructions = Vec::new();
            s.emit_phi(vec![(seq_exit, seq_val), (undef_bb, undef_val)])
        })
    }
}
