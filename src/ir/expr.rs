//! Expression Lowering

use super::*;

impl<'a> Lowerer<'a> {
    // ========================================================================
    // Expression Lowering
    // ========================================================================

    /// Lower an expression, returning the VarId holding the result.
    ///
    /// If `expr_guard_fail` is set on the lowerer, intrinsic emit helpers
    /// will guard their args against Undefined, jumping to the shared
    /// fail block. The caller that set `expr_guard_fail` is responsible
    /// for emitting the fail path and join Phi.
    pub fn lower_expression(&mut self, expr: &ast::Expr) -> VarId {
        self.set_span(expr.span);
        match &expr.node {
            ast::Expression::Literal(lit) => self.lower_literal(lit),

            ast::Expression::Variable(name) => {
                if let Some(var) = self.read_var(name) {
                    var
                } else if let Some(cv) = self.const_bindings.get(name).cloned() {
                    // Constant binding — emit inline literal
                    let lit = match &cv {
                        ConstValue::Bool(b) => Some(Literal::Bool(*b)),
                        ConstValue::UInt(n) => Some(Literal::UInt(*n)),
                        ConstValue::Int(n) => Some(Literal::Int(*n)),
                        ConstValue::Float(f) => Some(Literal::Float(*f)),
                        ConstValue::Text(s) => Some(Literal::Text(s.clone())),
                        ConstValue::Bytes(b) => Some(Literal::Bytes(b.clone())),
                        _ => None, // Array/Map constants can't be inlined as literals
                    };
                    if let Some(lit) = lit {
                        self.emit_const(lit)
                    } else {
                        self.error_placeholder()
                    }
                } else {
                    self.error_undefined_var(None, name, self.current_span);
                    self.error_placeholder()
                }
            }

            ast::Expression::QualifiedName { namespace, name } => {
                // Qualified constant access: ns::CONSTANT
                // Look up via require aliases → extern namespace
                if let Some(_extern_ns) = self.require_aliases.get(namespace) {
                    // TODO: ExternRegistry doesn't yet support constant registration.
                    // For now, treat as a zero-arg function call.
                    self.lower_function_call(Some(namespace), name, &[])
                } else {
                    self.error_undefined_var(Some(namespace), name, self.current_span);
                    self.error_placeholder()
                }
            }

            ast::Expression::BinaryOp { left, op, right } => self.lower_binary_op(left, op, right),

            ast::Expression::UnaryOp { op, operand } => self.lower_unary_op(op, operand),

            ast::Expression::FunctionCall {
                namespace,
                name,
                arguments,
            } => self.lower_function_call(namespace.as_ref(), name, arguments),

            ast::Expression::ArrayAccess { array, index } => {
                let base = self.lower_expression(array);
                let key = self.lower_expression(index);
                self.emit_index(base, key)
            }

            ast::Expression::MemberAccess { object, member } => {
                let base = self.lower_expression(object);
                let key = self.lower_expression(member);
                self.emit_index(base, key)
            }

            ast::Expression::Block {
                statements,
                final_expr,
            } => {
                self.push_scope();
                for stmt in statements {
                    self.lower_stmt(stmt);
                }
                let result = if let Some(expr) = final_expr {
                    self.lower_expression(expr)
                } else {
                    self.emit_undefined()
                };
                self.pop_scope();
                result
            }

            ast::Expression::If {
                conditions,
                then_block,
                then_expr,
                else_block,
                else_expr,
            } => {
                // Clear guard scope — if-else creates control flow that
                // can't share a fail_bb with the enclosing expression.
                let saved = self.expr_guard_fail.take();
                let result =
                    self.lower_if(conditions, then_block, then_expr, else_block, else_expr);
                self.expr_guard_fail = saved;
                result
            }

            ast::Expression::While {
                condition,
                body,
                body_expr,
            } => {
                let saved = self.expr_guard_fail.take();
                let result = self.lower_while(condition, body, body_expr);
                self.expr_guard_fail = saved;
                result
            }

            ast::Expression::Loop { body, body_expr } => {
                let saved = self.expr_guard_fail.take();
                let result = self.lower_loop(body, body_expr);
                self.expr_guard_fail = saved;
                result
            }

            ast::Expression::For {
                binding_is_value,
                binding,
                iterable,
                body,
                body_expr,
            } => {
                let saved = self.expr_guard_fail.take();
                let result = self.lower_for(*binding_is_value, binding, iterable, body, body_expr);
                self.expr_guard_fail = saved;
                result
            }

            ast::Expression::Match { value, arms } => {
                let saved = self.expr_guard_fail.take();
                let result = self.lower_match(value, arms);
                self.expr_guard_fail = saved;
                result
            }

            ast::Expression::Range {
                start,
                end,
                inclusive,
            } => self.lower_range(start, end, *inclusive),

            ast::Expression::Assignment { target, op, value } => {
                self.lower_assignment(target, op, value)
            }

            ast::Expression::Cast { value, target_type } => self.lower_cast(value, target_type),
        }
    }

    /// Lower a type cast expression (`value as Type`)
    fn lower_cast(&mut self, value: &ast::Expr, target_type: &ast::Identifier) -> VarId {
        let val = self.lower_expression(value);

        // Validate target is a castable numeric type
        let target = match target_type.as_ref() {
            "UInt" => types::NumericType::UInt,
            "Int" => types::NumericType::Int,
            "Float" => types::NumericType::Float,
            other => {
                self.diagnostics.error(
                    diagnostics::DiagnosticCode::E300_TypeMismatch,
                    self.current_span,
                    format!(
                        "cannot cast to '{}' (valid cast targets: UInt, Int, Float)",
                        other
                    ),
                );
                // Return undefined — error already emitted
                return self.emit_undefined();
            }
        };

        self.emit_unary_intrinsic(
            IntrinsicOp::Convert(target, types::ConvertMode::Unchecked),
            val,
        )
    }

    /// Lower a literal value
    pub fn lower_literal(&mut self, lit: &ast::Literal) -> VarId {
        match lit {
            ast::Literal::Bool(b) => self.emit_const(Literal::Bool(*b)),
            ast::Literal::UInt(n) => self.emit_const(Literal::UInt(*n)),
            ast::Literal::Int(n) => self.emit_const(Literal::Int(*n)),
            ast::Literal::Float(f) => self.emit_const(Literal::Float(*f)),
            ast::Literal::Text(s) => self.emit_const(Literal::Text(s.clone())),
            ast::Literal::Bytes(b) => self.emit_const(Literal::Bytes(b.clone())),
            ast::Literal::Array(elements) => self.lower_array_literal(elements),
            ast::Literal::Map(entries) => self.lower_map_literal(entries),
        }
    }

    fn lower_array_literal(&mut self, elements: &[ast::Expr]) -> VarId {
        let args: Vec<VarId> = elements.iter().map(|e| self.lower_expression(e)).collect();
        let dest = self.new_temp(TypeSet::array());
        self.emit(Instruction::Intrinsic {
            dest,
            op: IntrinsicOp::MakeArray,
            args,
        });
        dest
    }

    fn lower_map_literal(&mut self, entries: &[(ast::Expr, ast::Expr)]) -> VarId {
        let args: Vec<VarId> = entries
            .iter()
            .flat_map(|(k, v)| [self.lower_expression(k), self.lower_expression(v)])
            .collect();
        let dest = self.new_temp(TypeSet::map());
        self.emit(Instruction::Intrinsic {
            dest,
            op: IntrinsicOp::MakeMap,
            args,
        });
        dest
    }

    /// Set up an expression-level guard and evaluate an expression.
    /// If any intrinsic within the expression encounters an Undefined arg,
    /// all guards jump to a shared fail block. One Phi at the end merges
    /// the result with Undefined.
    ///
    /// If a guard is already active (nested expression), reuses it.
    fn lower_guarded_expression(&mut self, f: impl FnOnce(&mut Self) -> VarId) -> VarId {
        if self.expr_guard_fail.is_some() {
            // Already inside a guarded expression — just evaluate
            return f(self);
        }

        let fail_bb = self.fresh_block();
        self.expr_guard_fail = Some(fail_bb);

        let result = f(self);

        self.expr_guard_fail = None;

        // Check if any guard actually references fail_bb
        let fail_used = self
            .blocks
            .iter()
            .any(|b| b.terminator.successors().contains(&fail_bb));

        if !fail_used {
            // No guards were triggered — no Phi needed
            return result;
        }

        // Emit fail path and join
        let ok_exit = self.current_block;
        let join_bb = self.fresh_block();
        self.finish_block(Terminator::Jump { target: join_bb });

        self.current_block = fail_bb;
        self.current_instructions = Vec::new();
        let undef_val = self.emit_undefined();
        self.finish_block(Terminator::Jump { target: join_bb });

        self.current_block = join_bb;
        self.current_instructions = Vec::new();
        self.emit_phi(vec![(ok_exit, result), (fail_bb, undef_val)])
    }

    fn lower_binary_op(
        &mut self,
        left: &ast::Expr,
        op: &ast::BinaryOperator,
        right: &ast::Expr,
    ) -> VarId {
        // Short-circuit operators need special control flow
        match op {
            ast::BinaryOperator::And => return self.lower_short_circuit_and(left, right),
            ast::BinaryOperator::Or => return self.lower_short_circuit_or(left, right),
            _ => {}
        }

        self.lower_guarded_expression(|s| {
            let lhs = s.lower_expression(left);
            let rhs = s.lower_expression(right);

            match op {
                ast::BinaryOperator::NotEqual => {
                    let eq_result = s.emit_binary_intrinsic(IntrinsicOp::Eq, lhs, rhs);
                    s.emit_unary_intrinsic(IntrinsicOp::Not, eq_result)
                }
                ast::BinaryOperator::Greater => s.emit_binary_intrinsic(IntrinsicOp::Lt, rhs, lhs),
                ast::BinaryOperator::LessEqual => {
                    let lt_result = s.emit_binary_intrinsic(IntrinsicOp::Lt, rhs, lhs);
                    s.emit_unary_intrinsic(IntrinsicOp::Not, lt_result)
                }
                ast::BinaryOperator::GreaterEqual => {
                    let lt_result = s.emit_binary_intrinsic(IntrinsicOp::Lt, lhs, rhs);
                    s.emit_unary_intrinsic(IntrinsicOp::Not, lt_result)
                }
                _ => {
                    let intrinsic = op
                        .intrinsic_op()
                        .expect("reflexive/short-circuit ops handled above");
                    s.emit_binary_intrinsic(intrinsic, lhs, rhs)
                }
            }
        })
    }

    // emit_binary_intrinsic and emit_unary_intrinsic are defined in mod.rs

    fn lower_short_circuit_and(&mut self, left: &ast::Expr, right: &ast::Expr) -> VarId {
        let lhs = self.lower_expression(left);

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        let from_left = self.current_block;
        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: right_block,
            else_target: join_block,
            span: self.current_span,
        });

        self.current_block = right_block;
        self.current_instructions = Vec::new();
        let rhs = self.lower_expression(right);
        let from_right = self.current_block;
        self.finish_block(Terminator::Jump { target: join_block });

        self.current_block = join_block;
        self.current_instructions = Vec::new();

        let false_var = self.emit_const(Literal::Bool(false));

        let result = self.new_temp(TypeSet::bool());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(from_left, false_var), (from_right, rhs)],
        });

        result
    }

    fn lower_short_circuit_or(&mut self, left: &ast::Expr, right: &ast::Expr) -> VarId {
        let lhs = self.lower_expression(left);

        let right_block = self.fresh_block();
        let join_block = self.fresh_block();

        let from_left = self.current_block;
        self.finish_block(Terminator::If {
            condition: lhs,
            then_target: join_block,
            else_target: right_block,
            span: self.current_span,
        });

        self.current_block = right_block;
        self.current_instructions = Vec::new();
        let rhs = self.lower_expression(right);
        let from_right = self.current_block;
        self.finish_block(Terminator::Jump { target: join_block });

        self.current_block = join_block;
        self.current_instructions = Vec::new();

        let true_var = self.emit_const(Literal::Bool(true));

        let result = self.new_temp(TypeSet::bool());
        self.emit(Instruction::Phi {
            dest: result,
            sources: vec![(from_left, true_var), (from_right, rhs)],
        });

        result
    }

    fn lower_unary_op(&mut self, op: &ast::UnaryOperator, operand: &ast::Expr) -> VarId {
        self.lower_guarded_expression(|s| {
            let arg = s.lower_expression(operand);
            s.emit_unary_intrinsic(op.intrinsic_op(), arg)
        })
    }

    pub fn lower_function_call(
        &mut self,
        namespace: Option<&ast::Identifier>,
        name: &ast::Identifier,
        arguments: &[ast::Expr],
    ) -> VarId {
        // Check for compiler intrinsics first (e.g. len).
        // These lower to Instruction::Intrinsic, not function calls.
        if namespace.is_none()
            && let Some(result) = self.try_lower_intrinsic(name, arguments)
        {
            return result;
        }

        // Resolve the extern function and effective namespace.
        //
        // Resolution order for unqualified calls:
        //   1. Intrinsics (handled above by try_lower_intrinsic)
        //   2. Local user functions (same file)
        //   3. Merged imports (import "x" as _)
        //   4. Externs — global and merged (require ns as _)
        //
        // Locals and imports override externs — the original
        // is always reachable via qualified `ns::func()` syntax.
        //
        // For qualified calls (ns::func): look up the alias in require_aliases
        // to find the extern namespace, then look up the function in that namespace.
        //
        // `effective_ns` captures the resolved namespace for unqualified calls
        // that match a merged import — the Call's FunctionRef needs it.
        // Resolve the call target and extract param metadata.
        // We extract owned data from the extern def to avoid holding a borrow
        // on `self` across the mutable operations that follow.
        let mut effective_ns = namespace.cloned();
        let has_root_extern = self.lookup_root_extern(name).is_some();

        let (param_by_ref, param_specs_owned): (
            Option<Vec<bool>>,
            Option<Vec<crate::externs::ParamSpec>>,
        ) = if let Some(ns) = namespace {
            // Qualified call: ns::func()
            if let Some(extern_ns) = self.require_aliases.get(ns) {
                let def = self.externs.get_in(extern_ns, name);
                let specs = def.map(|d| d.meta.params.clone());
                let by_ref = specs.as_ref().map(|s| s.iter().map(|p| p.by_ref).collect());
                (by_ref, specs)
            } else {
                (self.user_fn_params.get(name).cloned(), None)
            }
        } else if self.user_fn_params.contains_key(name) {
            // Local function — shadows any externs/imports
            if has_root_extern {
                self.diagnostics.warning(
                    diagnostics::DiagnosticCode::W004_ShadowedVariable,
                    self.current_span,
                    format!("local function `{}` shadows extern function", name),
                );
            } else if self.merged_imports.contains_key(name) {
                self.diagnostics.warning(
                    diagnostics::DiagnosticCode::W004_ShadowedVariable,
                    self.current_span,
                    format!("local function `{}` shadows imported function", name),
                );
            }
            (self.user_fn_params.get(name).cloned(), None)
        } else if let Some(canonical_ns) = self.merged_imports.get(name).cloned() {
            // Merged import — shadows externs
            if has_root_extern {
                self.diagnostics.warning(
                    diagnostics::DiagnosticCode::W004_ShadowedVariable,
                    self.current_span,
                    format!("imported function `{}` shadows extern function", name),
                );
            }
            effective_ns = Some(canonical_ns);
            (self.user_fn_params.get(name).cloned(), None)
        } else {
            // Merged externs (require ns as _) — set namespace for linker resolution
            if let Some(src_ns) = self.merged_externs.get(name).cloned() {
                effective_ns = Some(src_ns);
            }
            let def = self.lookup_root_extern(name);
            let specs = def.map(|d| d.meta.params.clone());
            let by_ref = specs.as_ref().map(|s| s.iter().map(|p| p.by_ref).collect());
            (
                by_ref.or_else(|| self.user_fn_params.get(name).cloned()),
                specs,
            )
        };

        let param_specs = param_specs_owned.as_deref();

        // Lower each arg, wrapping in MakeRef for by-ref callee params.
        // The caller emits the coercion: MakeRef for by-ref, plain value for by-val.
        let args: Vec<VarId> = arguments
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let arg_var = self.lower_expression(arg);
                let is_ref = param_by_ref
                    .as_ref()
                    .and_then(|modes| modes.get(i))
                    .copied()
                    .unwrap_or(false);
                if is_ref {
                    // Wrap in MakeRef — creates Slot::Ref at runtime.
                    // If the arg is a by-ref param variable, use the PARAM's
                    // VarId as the base (its slot has the Ref chain to the
                    // original caller). Otherwise use the lowered value.
                    let base = if let ast::Expression::Variable(name) = &arg.node {
                        self.byref_param_vars.get(name).copied().unwrap_or(arg_var)
                    } else {
                        arg_var
                    };
                    let ref_dest = self.new_temp(TypeSet::any());
                    self.emit(Instruction::MakeRef {
                        dest: ref_dest,
                        base,
                    });
                    ref_dest
                } else {
                    arg_var
                }
            })
            .collect();

        // Insert type guards for extern params with type constraints.
        let has_type_guards = param_specs.is_some_and(|specs| {
            specs
                .iter()
                .any(|s| !s.type_sig.is_empty() && s.type_sig != TypeSet::any())
        });

        if has_type_guards {
            let skip_bb = self.fresh_block();
            let call_bb = self.fresh_block();
            let join_bb = self.fresh_block();

            // Emit a Match guard for each constrained param
            for (i, arg) in args.iter().enumerate() {
                let type_sig = param_specs
                    .and_then(|specs| specs.get(i))
                    .map(|s| s.type_sig)
                    .unwrap_or(TypeSet::any());

                if type_sig.is_empty() || type_sig == TypeSet::any() {
                    continue; // no constraint
                }

                // Build Match arms: one per type in the sig, all going to next check
                let next_bb = self.fresh_block();
                let match_arms: Vec<(MatchPattern, BlockId)> = type_sig
                    .iter()
                    .map(|ty| (MatchPattern::Type(ty), next_bb))
                    .collect();

                self.finish_block(Terminator::Match {
                    value: *arg,
                    arms: match_arms,
                    default: skip_bb,
                    span: self.current_span,
                });

                self.current_block = next_bb;
                self.current_instructions = Vec::new();
            }

            // All guards passed — jump to call block
            self.finish_block(Terminator::Jump { target: call_bb });

            // Call block
            self.current_block = call_bb;
            self.current_instructions = Vec::new();
            let call_dest = self.emit_call(
                FunctionRef {
                    namespace: effective_ns.clone(),
                    name: name.clone(),
                },
                args.clone(),
            );
            // Reload by-ref args after call
            self.emit_byref_reloads(arguments, &args, &param_by_ref);
            let call_exit = self.current_block;
            self.finish_block(Terminator::Jump { target: join_bb });

            // Skip block — type mismatch, result is undefined
            self.current_block = skip_bb;
            self.current_instructions = Vec::new();
            let undef_dest = self.emit_undefined();
            let skip_exit = self.current_block;
            self.finish_block(Terminator::Jump { target: join_bb });

            // Join with phi
            self.current_block = join_bb;
            self.current_instructions = Vec::new();
            self.emit_phi(vec![(call_exit, call_dest), (skip_exit, undef_dest)])
        } else {
            // No type constraints — emit call directly
            let result = self.emit_call(
                FunctionRef {
                    namespace: effective_ns.clone(),
                    name: name.clone(),
                },
                args.clone(),
            );
            // Reload by-ref args after call
            self.emit_byref_reloads(arguments, &args, &param_by_ref);
            result
        }
    }

    /// Try to lower a call as a compiler intrinsic.
    /// Returns Some(result) if recognized, None to fall through to normal call resolution.
    fn try_lower_intrinsic(&mut self, name: &str, arguments: &[ast::Expr]) -> Option<VarId> {
        match name {
            "len" if arguments.len() == 1 => {
                let arg = self.lower_expression(&arguments[0]);
                Some(self.emit_unary_intrinsic(IntrinsicOp::Len, arg))
            }
            "collect" if arguments.len() == 1 => {
                let arg = self.lower_expression(&arguments[0]);
                Some(self.emit_unary_intrinsic(IntrinsicOp::Collect, arg))
            }
            "append" if arguments.len() == 2 => {
                let arr = self.lower_expression(&arguments[0]);
                let val = self.lower_expression(&arguments[1]);
                let dest = self.new_temp(TypeSet::array());
                self.emit(Instruction::Append {
                    dest,
                    arr,
                    value: val,
                });
                // Reassign the array slot — append mutates via CoW, so the
                // slot must be updated to the post-mutation value.
                if let ast::Expression::Variable(name) = &arguments[0].node {
                    self.reassign(name, dest);
                }
                Some(dest)
            }
            _ => None,
        }
    }

    /// Lower an expression for a `with` binding, extracting ref origin if the
    /// expression is an indexed access.
    ///
    /// Returns `(value_var, ref_origin)`:
    /// - For `arr[i]` / `obj.field`: emits MakeRef, returns `Some(RefOrigin)`
    /// - For plain variables: emits MakeRef (whole-value), returns `Some(RefOrigin)`
    /// - For other expressions: returns `(value, None)` — no ref tracking
    pub fn lower_ref_expression(&mut self, expr: &ast::Expr) -> (VarId, Option<RefOrigin>) {
        match &expr.node {
            ast::Expression::ArrayAccess { array, index } => {
                let base_name = if let ast::Expression::Variable(name) = &array.node {
                    Some(name.clone())
                } else {
                    None
                };
                let base = self.lower_expression(array);
                let key = self.lower_expression(index);
                let dest = self.new_temp(TypeSet::any());
                self.emit(Instruction::MakeAccessor { dest, base, key });
                let origin = RefOrigin {
                    ref_var: dest,
                    base_var: base,
                    key_var: Some(key),
                    base_name,
                };
                (dest, Some(origin))
            }

            ast::Expression::MemberAccess { object, member } => {
                let base_name = if let ast::Expression::Variable(name) = &object.node {
                    Some(name.clone())
                } else {
                    None
                };
                let base = self.lower_expression(object);
                let key = self.lower_expression(member);
                let dest = self.new_temp(TypeSet::any());
                self.emit(Instruction::MakeAccessor { dest, base, key });
                let origin = RefOrigin {
                    ref_var: dest,
                    base_var: base,
                    key_var: Some(key),
                    base_name,
                };
                (dest, Some(origin))
            }

            ast::Expression::Variable(name) => {
                if let Some(var) = self.read_var(name) {
                    let dest = self.new_temp(TypeSet::any());
                    self.emit(Instruction::MakeRef { dest, base: var });
                    let origin = RefOrigin {
                        ref_var: dest,
                        base_var: var,
                        key_var: None,
                        base_name: Some(name.clone()),
                    };
                    (dest, Some(origin))
                } else {
                    // Fall through to normal lowering (will emit error)
                    let var = self.lower_expression(expr);
                    (var, None)
                }
            }

            // For complex expressions (function calls, blocks, etc.),
            // there's no location to write back to.
            _ => {
                let var = self.lower_expression(expr);
                (var, None)
            }
        }
    }

    /// Emit Reload + Assign for by-ref args after a function call.
    /// For each arg that is a named variable AND the callee param is by-ref,
    /// emit a Reload to create a fresh SSA def (the callee may have mutated
    /// the value in-place through the Slot::Ref).
    fn emit_byref_reloads(
        &mut self,
        ast_args: &[ast::Expr],
        ir_args: &[VarId],
        param_by_ref: &Option<Vec<bool>>,
    ) {
        let Some(by_ref_modes) = param_by_ref else {
            return;
        };
        for (i, ast_arg) in ast_args.iter().enumerate() {
            let is_ref = by_ref_modes.get(i).copied().unwrap_or(false);
            if !is_ref {
                continue;
            }
            // Only reload named variables — computed expressions have no
            // slot to reassign to.
            if let ast::Expression::Variable(name) = &ast_arg.node
                && let Some(&arg_var) = ir_args.get(i)
            {
                let reloaded = self.emit_reload(arg_var);
                self.reassign(name, reloaded);
            }
        }
    }
}
