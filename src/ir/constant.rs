//! Constant Expression Lowering
//!
//! Evaluates constant expressions at compile time. This is used for:
//! - `const` declarations
//! - Compile-time evaluation of pure expressions with literal arguments
//!
//! Const evaluation delegates to builtin const evaluators registered in
//! the BuiltinRegistry.

use super::*;

impl<'a> Lowerer<'a> {
    /// Lower a constant declaration
    pub(super) fn lower_constant(&mut self, constant: &ast::Constant) -> Result<Vec<ConstBinding>> {
        // Evaluate the initializer expression at compile time
        let value = self.const_eval_expr(&constant.value)?;

        // Match the pattern and create bindings
        let mut bindings = Vec::new();
        self.const_match_pattern(&constant.pattern.node, &value, &mut bindings)?;

        // Add all bindings to const_bindings for future reference
        for binding in &bindings {
            self.const_bindings
                .insert(binding.name.0.clone(), binding.value.clone());
        }

        Ok(bindings)
    }

    // ========================================================================
    // Constant Expression Evaluation
    // ========================================================================

    /// Evaluate an expression at compile time, returning a ConstValue
    fn const_eval_expr(&self, expr: &ast::Expression) -> Result<ConstValue> {
        match expr {
            ast::Expression::Literal(lit) => self.const_eval_literal(lit),

            ast::Expression::Variable(name) => {
                // Look up in const bindings
                self.const_bindings
                    .get(&name.0)
                    .cloned()
                    .ok_or_else(|| LowerError::SemanticError {
                        message: format!("cannot use variable '{}' in constant expression", name.0),
                        span: dummy_span(),
                    })
            }

            ast::Expression::QualifiedName { namespace, name } => {
                // TODO: Look up in imported module's constants
                Err(LowerError::SemanticError {
                    message: format!(
                        "namespaced constant '{}::{}' not yet supported",
                        namespace.0, name.0
                    ),
                    span: dummy_span(),
                })
            }

            ast::Expression::BinaryOp { left, op, right } => {
                self.const_eval_binary_op(left, op, right)
            }

            ast::Expression::UnaryOp { op, operand } => self.const_eval_unary_op(op, operand),

            ast::Expression::FunctionCall {
                namespace,
                name,
                arguments,
            } => self.const_eval_call(namespace.as_ref(), name, arguments),

            ast::Expression::ArrayAccess { array, index } => {
                let arr = self.const_eval_expr(array)?;
                let idx = self.const_eval_expr(index)?;
                self.const_index(&arr, &idx)
            }

            ast::Expression::MemberAccess { object, member } => {
                let obj = self.const_eval_expr(object)?;
                let key = self.const_eval_expr(member)?;
                self.const_index(&obj, &key)
            }

            // Control flow is not allowed in const expressions
            // TODO: Could support if/match/block by evaluating at compile time,
            // but loops would need termination analysis. Keep it simple for now.
            ast::Expression::Block { .. }
            | ast::Expression::If { .. }
            | ast::Expression::While { .. }
            | ast::Expression::Loop { .. }
            | ast::Expression::For { .. }
            | ast::Expression::Match { .. }
            | ast::Expression::Range { .. } => Err(LowerError::SemanticError {
                message: "control flow not allowed in constant expression".to_string(),
                span: dummy_span(),
            }),
        }
    }

    /// Convert a literal to a ConstValue
    fn const_eval_literal(&self, lit: &ast::Literal) -> Result<ConstValue> {
        match lit {
            ast::Literal::Bool(b) => Ok(ConstValue::Bool(*b)),
            ast::Literal::UInt(n) => Ok(ConstValue::UInt(*n)),
            ast::Literal::Int(n) => Ok(ConstValue::Int(*n)),
            ast::Literal::Float(f) => Ok(ConstValue::Float(*f)),
            ast::Literal::Text(s) => Ok(ConstValue::Text(s.clone())),
            ast::Literal::Bytes(b) => Ok(ConstValue::Bytes(b.clone())),
            ast::Literal::Array(elements) => {
                let values: Result<Vec<_>> =
                    elements.iter().map(|e| self.const_eval_expr(e)).collect();
                Ok(ConstValue::Array(values?))
            }
            ast::Literal::Map(entries) => {
                let pairs: Result<Vec<_>> = entries
                    .iter()
                    .map(|(k, v)| Ok((self.const_eval_expr(k)?, self.const_eval_expr(v)?)))
                    .collect();
                Ok(ConstValue::Map(pairs?))
            }
        }
    }

    /// Evaluate a binary operation at compile time
    fn const_eval_binary_op(
        &self,
        left: &ast::Expression,
        op: &ast::BinaryOperator,
        right: &ast::Expression,
    ) -> Result<ConstValue> {
        // Short-circuit evaluation for && and ||
        match op {
            ast::BinaryOperator::And => {
                let lhs = self.const_eval_expr(left)?;
                if let ConstValue::Bool(false) = lhs {
                    return Ok(ConstValue::Bool(false));
                }
                return self.const_eval_expr(right);
            }
            ast::BinaryOperator::Or => {
                let lhs = self.const_eval_expr(left)?;
                if let ConstValue::Bool(true) = lhs {
                    return Ok(ConstValue::Bool(true));
                }
                return self.const_eval_expr(right);
            }
            _ => {}
        }

        let lhs = self.const_eval_expr(left)?;
        let rhs = self.const_eval_expr(right)?;

        // Map operator to builtin name
        let builtin_name = match op {
            ast::BinaryOperator::Add => "core.add",
            ast::BinaryOperator::Subtract => "core.sub",
            ast::BinaryOperator::Multiply => "core.mul",
            ast::BinaryOperator::Divide => "core.div",
            ast::BinaryOperator::Modulo => "core.mod",
            ast::BinaryOperator::Equal => "core.eq",
            ast::BinaryOperator::NotEqual => "core.neq",
            ast::BinaryOperator::Less => "core.lt",
            ast::BinaryOperator::LessEqual => "core.le",
            ast::BinaryOperator::Greater => "core.gt",
            ast::BinaryOperator::GreaterEqual => "core.ge",
            ast::BinaryOperator::BitwiseAnd => "core.bit_and",
            ast::BinaryOperator::BitwiseOr => "core.bit_or",
            ast::BinaryOperator::BitwiseXor => "core.bit_xor",
            ast::BinaryOperator::ShiftLeft => "core.shl",
            ast::BinaryOperator::ShiftRight => "core.shr",
            ast::BinaryOperator::And | ast::BinaryOperator::Or => unreachable!(),
        };

        self.call_const_builtin(builtin_name, &[lhs, rhs])
    }

    /// Evaluate a unary operation at compile time
    fn const_eval_unary_op(
        &self,
        op: &ast::UnaryOperator,
        operand: &ast::Expression,
    ) -> Result<ConstValue> {
        let arg = self.const_eval_expr(operand)?;

        let builtin_name = match op {
            ast::UnaryOperator::Negate => "core.neg",
            ast::UnaryOperator::Not => "core.not",
            ast::UnaryOperator::BitwiseNot => "core.bit_not",
        };

        self.call_const_builtin(builtin_name, &[arg])
    }

    /// Evaluate a function call at compile time
    fn const_eval_call(
        &self,
        namespace: Option<&ast::Identifier>,
        name: &ast::Identifier,
        arguments: &[ast::Expression],
    ) -> Result<ConstValue> {
        // Evaluate all arguments first
        let args: Result<Vec<_>> = arguments.iter().map(|e| self.const_eval_expr(e)).collect();
        let args = args?;

        // Build the full function name
        let full_name = match namespace {
            Some(ns) => format!("{}.{}", ns.0, name.0),
            None => name.0.clone(),
        };

        self.call_const_builtin(&full_name, &args)
    }

    /// Call a builtin's const evaluator
    fn call_const_builtin(&self, name: &str, args: &[ConstValue]) -> Result<ConstValue> {
        // Look up the builtin
        let builtin = self
            .builtins
            .get(name)
            .ok_or_else(|| LowerError::SemanticError {
                message: format!("unknown function '{}' in constant expression", name),
                span: dummy_span(),
            })?;

        // Check if it's a const function
        let const_eval =
            builtin
                .meta
                .purity
                .const_eval()
                .ok_or_else(|| LowerError::SemanticError {
                    message: format!("function '{}' cannot be used in constant expression", name),
                    span: dummy_span(),
                })?;

        // Call the const evaluator
        const_eval(args).ok_or_else(|| LowerError::SemanticError {
            message: format!("constant evaluation of '{}' failed", name),
            span: dummy_span(),
        })
    }

    /// Index into a const array or map
    fn const_index(&self, base: &ConstValue, key: &ConstValue) -> Result<ConstValue> {
        match (base, key) {
            (ConstValue::Array(arr), ConstValue::UInt(idx)) => arr
                .get(*idx as usize)
                .cloned()
                .ok_or_else(|| LowerError::SemanticError {
                    message: format!("array index {} out of bounds", idx),
                    span: dummy_span(),
                }),
            (ConstValue::Map(entries), key) => {
                for (k, v) in entries {
                    if k == key {
                        return Ok(v.clone());
                    }
                }
                Err(LowerError::SemanticError {
                    message: "key not found in map".to_string(),
                    span: dummy_span(),
                })
            }
            _ => Err(LowerError::SemanticError {
                message: "invalid indexing in constant expression".to_string(),
                span: dummy_span(),
            }),
        }
    }

    /// Match a pattern against a const value and produce bindings
    fn const_match_pattern(
        &self,
        pattern: &ast::Pattern,
        value: &ConstValue,
        bindings: &mut Vec<ConstBinding>,
    ) -> Result<()> {
        match pattern {
            ast::Pattern::Wildcard => {
                // Ignore the value
                Ok(())
            }

            ast::Pattern::Variable(name) => {
                bindings.push(ConstBinding {
                    name: name.clone(),
                    value: value.clone(),
                });
                Ok(())
            }

            ast::Pattern::Literal(lit) => {
                // Check that the value matches the literal
                let expected = self.const_eval_literal(lit)?;
                if value == &expected {
                    Ok(())
                } else {
                    Err(LowerError::SemanticError {
                        message: "constant pattern match failed".to_string(),
                        span: dummy_span(),
                    })
                }
            }

            ast::Pattern::Array(patterns) => {
                if let ConstValue::Array(elements) = value {
                    if elements.len() != patterns.len() {
                        return Err(LowerError::SemanticError {
                            message: format!(
                                "array pattern has {} elements but value has {}",
                                patterns.len(),
                                elements.len()
                            ),
                            span: dummy_span(),
                        });
                    }
                    for (pat, val) in patterns.iter().zip(elements.iter()) {
                        self.const_match_pattern(&pat.node, val, bindings)?;
                    }
                    Ok(())
                } else {
                    Err(LowerError::SemanticError {
                        message: "expected array in constant pattern".to_string(),
                        span: dummy_span(),
                    })
                }
            }

            ast::Pattern::ArrayRest {
                before,
                rest,
                after,
            } => {
                if let ConstValue::Array(elements) = value {
                    let min_len = before.len() + after.len();
                    if elements.len() < min_len {
                        return Err(LowerError::SemanticError {
                            message: format!(
                                "array pattern requires at least {} elements but value has {}",
                                min_len,
                                elements.len()
                            ),
                            span: dummy_span(),
                        });
                    }

                    // Match before patterns
                    for (pat, val) in before.iter().zip(elements.iter()) {
                        self.const_match_pattern(&pat.node, val, bindings)?;
                    }

                    // Match after patterns (from the end)
                    let after_start = elements.len() - after.len();
                    for (pat, val) in after.iter().zip(elements[after_start..].iter()) {
                        self.const_match_pattern(&pat.node, val, bindings)?;
                    }

                    // Bind rest if present
                    if let Some(rest_name) = rest {
                        let rest_elements = elements[before.len()..after_start].to_vec();
                        bindings.push(ConstBinding {
                            name: rest_name.clone(),
                            value: ConstValue::Array(rest_elements),
                        });
                    }

                    Ok(())
                } else {
                    Err(LowerError::SemanticError {
                        message: "expected array in constant pattern".to_string(),
                        span: dummy_span(),
                    })
                }
            }

            ast::Pattern::Map(entries) => {
                if let ConstValue::Map(map_entries) = value {
                    for (key_pat, val_pat) in entries {
                        // Key pattern must be a literal for const matching
                        let key = match &key_pat.node {
                            ast::Pattern::Literal(lit) => self.const_eval_literal(lit)?,
                            _ => {
                                return Err(LowerError::SemanticError {
                                    message: "map pattern key must be a literal".to_string(),
                                    span: dummy_span(),
                                });
                            }
                        };

                        // Find the value in the map
                        let map_value = map_entries
                            .iter()
                            .find(|(k, _)| k == &key)
                            .map(|(_, v)| v)
                            .ok_or_else(|| LowerError::SemanticError {
                                message: "key not found in map constant".to_string(),
                                span: dummy_span(),
                            })?;

                        self.const_match_pattern(&val_pat.node, map_value, bindings)?;
                    }
                    Ok(())
                } else {
                    Err(LowerError::SemanticError {
                        message: "expected map in constant pattern".to_string(),
                        span: dummy_span(),
                    })
                }
            }

            ast::Pattern::Type { type_name, binding } => {
                // Check type matches
                let type_matches = matches!(
                    (type_name.0.as_str(), value),
                    ("Bool", ConstValue::Bool(_))
                        | ("UInt", ConstValue::UInt(_))
                        | ("Int", ConstValue::Int(_))
                        | ("Float", ConstValue::Float(_))
                        | ("Text", ConstValue::Text(_))
                        | ("Bytes", ConstValue::Bytes(_))
                        | ("Array", ConstValue::Array(_))
                        | ("Map", ConstValue::Map(_))
                );

                if !type_matches {
                    return Err(LowerError::SemanticError {
                        message: format!("type pattern '{}' does not match value", type_name.0),
                        span: dummy_span(),
                    });
                }

                // Match inner binding if present
                if let Some(inner) = binding {
                    self.const_match_pattern(&inner.node, value, bindings)?;
                }

                Ok(())
            }
        }
    }
}
