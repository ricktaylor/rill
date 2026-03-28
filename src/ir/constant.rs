//! Constant Expression Lowering
//!
//! Evaluates constant expressions at compile time. This is used for:
//! - `const` declarations
//! - Compile-time evaluation of pure expressions with literal arguments
//!
//! Const evaluation delegates to extern const evaluators registered in
//! the ExternRegistry. Shared const evaluation utilities are in the
//! `const_eval` module.

use super::*;
use crate::ir::const_eval;

/// Internal result type for const evaluation
type ConstResult<T> = std::result::Result<T, String>;

impl<'a> Lowerer<'a> {
    /// Lower a constant declaration
    ///
    /// Returns `Some(bindings)` on success, `None` on error (with diagnostic emitted).
    pub fn lower_constant(&mut self, constant: &ast::Constant) -> Option<Vec<ConstBinding>> {
        // Evaluate the initializer expression at compile time
        let value = match self.const_eval_expr(&constant.value) {
            Ok(v) => v,
            Err(msg) => {
                self.error_const_eval(&msg, self.current_span);
                return None;
            }
        };

        // Match the pattern and create bindings
        let mut bindings = Vec::new();
        if let Err(msg) = self.const_match_pattern(&constant.pattern.node, &value, &mut bindings) {
            self.error_const_eval(&msg, self.current_span);
            return None;
        }

        // Add all bindings to const_bindings for future reference
        for binding in &bindings {
            self.const_bindings
                .insert(binding.name.clone(), binding.value.clone());
        }

        Some(bindings)
    }

    // ========================================================================
    // Constant Expression Evaluation
    // ========================================================================

    /// Evaluate an expression at compile time, returning a ConstValue
    fn const_eval_expr(&self, expr: &ast::Expression) -> ConstResult<ConstValue> {
        match expr {
            ast::Expression::Literal(lit) => self.const_eval_literal(lit),

            ast::Expression::Variable(name) => {
                // Look up in const bindings
                self.const_bindings
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("cannot use variable '{}' in constant expression", name))
            }

            ast::Expression::QualifiedName { namespace, name } => {
                // TODO: Look up in imported module's constants
                Err(format!(
                    "namespaced constant '{}::{}' not yet supported",
                    namespace, name
                ))
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
                self.const_index_expr(&arr, &idx)
            }

            ast::Expression::MemberAccess { object, member } => {
                let obj = self.const_eval_expr(object)?;
                let key = self.const_eval_expr(member)?;
                self.const_index_expr(&obj, &key)
            }

            ast::Expression::Cast { value, target_type } => {
                let val = self.const_eval_expr(value)?;
                let target = match target_type.as_ref() {
                    "UInt" => 1u64,
                    "Int" => 2u64,
                    "Float" => 3u64,
                    other => {
                        return Err(format!(
                            "cannot cast to '{}' (valid cast targets: UInt, Int, Float)",
                            other
                        ));
                    }
                };
                const_eval::eval_intrinsic_const(
                    crate::ir::IntrinsicOp::Cast,
                    &[val, ConstValue::UInt(target)],
                )
                .ok_or_else(|| "cast failed: incompatible source type".to_string())
            }

            // Control flow and assignment are not allowed in const expressions
            ast::Expression::Block { .. }
            | ast::Expression::If { .. }
            | ast::Expression::While { .. }
            | ast::Expression::Loop { .. }
            | ast::Expression::For { .. }
            | ast::Expression::Match { .. }
            | ast::Expression::Range { .. } => {
                Err("control flow not allowed in constant expression".to_string())
            }

            // Assignment has side effects - not allowed in const expressions
            ast::Expression::Assignment { .. } => {
                Err("assignment not allowed in constant expression".to_string())
            }
        }
    }

    /// Convert a literal to a ConstValue
    fn const_eval_literal(&self, lit: &ast::Literal) -> ConstResult<ConstValue> {
        match lit {
            ast::Literal::Bool(b) => Ok(ConstValue::Bool(*b)),
            ast::Literal::UInt(n) => Ok(ConstValue::UInt(*n)),
            ast::Literal::Int(n) => Ok(ConstValue::Int(*n)),
            ast::Literal::Float(f) => Ok(ConstValue::Float(*f)),
            ast::Literal::Text(s) => Ok(ConstValue::Text(s.clone())),
            ast::Literal::Bytes(b) => Ok(ConstValue::Bytes(b.clone())),
            ast::Literal::Array(elements) => {
                let values: ConstResult<Vec<_>> =
                    elements.iter().map(|e| self.const_eval_expr(e)).collect();
                Ok(ConstValue::Array(values?))
            }
            ast::Literal::Map(entries) => {
                let pairs: ConstResult<Vec<_>> = entries
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
    ) -> ConstResult<ConstValue> {
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

        // Reflexive comparison operators expand to combinations of Eq/Lt/Not
        match op {
            // a != b  →  Not(Eq(a, b))
            ast::BinaryOperator::NotEqual => {
                let eq_result = self.eval_intrinsic(IntrinsicOp::Eq, &[lhs, rhs])?;
                return self.eval_intrinsic(IntrinsicOp::Not, &[eq_result]);
            }
            // a > b  →  Lt(b, a)  (swap operands)
            ast::BinaryOperator::Greater => {
                return self.eval_intrinsic(IntrinsicOp::Lt, &[rhs, lhs]);
            }
            // a <= b  →  Not(Lt(b, a))
            ast::BinaryOperator::LessEqual => {
                let lt_result = self.eval_intrinsic(IntrinsicOp::Lt, &[rhs, lhs])?;
                return self.eval_intrinsic(IntrinsicOp::Not, &[lt_result]);
            }
            // a >= b  →  Not(Lt(a, b))
            ast::BinaryOperator::GreaterEqual => {
                let lt_result = self.eval_intrinsic(IntrinsicOp::Lt, &[lhs, rhs])?;
                return self.eval_intrinsic(IntrinsicOp::Not, &[lt_result]);
            }
            _ => {}
        }

        // Direct intrinsic mapping for remaining operators
        let intrinsic = op
            .intrinsic_op()
            .expect("reflexive/short-circuit ops handled above");
        self.eval_intrinsic(intrinsic, &[lhs, rhs])
    }

    /// Evaluate a unary operation at compile time
    fn const_eval_unary_op(
        &self,
        op: &ast::UnaryOperator,
        operand: &ast::Expression,
    ) -> ConstResult<ConstValue> {
        let arg = self.const_eval_expr(operand)?;
        self.eval_intrinsic(op.intrinsic_op(), &[arg])
    }

    /// Evaluate a function call at compile time
    fn const_eval_call(
        &self,
        namespace: Option<&ast::Identifier>,
        name: &ast::Identifier,
        arguments: &[ast::Expression],
    ) -> ConstResult<ConstValue> {
        // Evaluate all arguments first
        let args: ConstResult<Vec<_>> = arguments.iter().map(|e| self.const_eval_expr(e)).collect();
        let args = args?;

        // Check for known intrinsic functions first (no registry lookup needed)
        if namespace.is_none()
            && let Some(op) = intrinsic_by_name(name)
        {
            return self.eval_intrinsic(op, &args);
        }

        // Fall through to registry lookup for host-provided externs
        let lookup_name = match namespace {
            Some(ns) => format!("{}::{}", ns, name),
            None => name.to_string(),
        };

        self.call_const_extern(&lookup_name, &args)
    }

    /// Evaluate an intrinsic at compile time
    fn eval_intrinsic(&self, op: IntrinsicOp, args: &[ConstValue]) -> ConstResult<ConstValue> {
        const_eval::eval_intrinsic_const(op, args)
            .ok_or_else(|| format!("constant evaluation of {:?} failed", op))
    }

    /// Call an extern's const evaluator
    fn call_const_extern(&self, name: &str, args: &[ConstValue]) -> ConstResult<ConstValue> {
        // Look up the extern
        let def = self
            .externs
            .get(name)
            .ok_or_else(|| format!("unknown function '{}' in constant expression", name))?;

        // Check if it's a const function
        let const_eval =
            def.meta.purity.const_eval().ok_or_else(|| {
                format!("function '{}' cannot be used in constant expression", name)
            })?;

        // Call the const evaluator
        const_eval(args).ok_or_else(|| format!("constant evaluation of '{}' failed", name))
    }

    /// Index into a const array or map
    fn const_index_expr(&self, base: &ConstValue, key: &ConstValue) -> ConstResult<ConstValue> {
        const_eval::const_index(base, key)
            .ok_or_else(|| "indexing failed in constant expression".to_string())
    }

    /// Match a pattern against a const value and produce bindings
    fn const_match_pattern(
        &self,
        pattern: &ast::Pattern,
        value: &ConstValue,
        bindings: &mut Vec<ConstBinding>,
    ) -> ConstResult<()> {
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
                    Err("constant pattern match failed".to_string())
                }
            }

            ast::Pattern::Array(patterns) => {
                if let ConstValue::Array(elements) = value {
                    if elements.len() != patterns.len() {
                        return Err(format!(
                            "array pattern has {} elements but value has {}",
                            patterns.len(),
                            elements.len()
                        ));
                    }
                    for (pat, val) in patterns.iter().zip(elements.iter()) {
                        self.const_match_pattern(&pat.node, val, bindings)?;
                    }
                    Ok(())
                } else {
                    Err("expected array in constant pattern".to_string())
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
                        return Err(format!(
                            "array pattern requires at least {} elements but value has {}",
                            min_len,
                            elements.len()
                        ));
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
                    Err("expected array in constant pattern".to_string())
                }
            }

            ast::Pattern::Map(entries) => {
                if let ConstValue::Map(map_entries) = value {
                    for (key_pat, val_pat) in entries {
                        // Key pattern must be a literal for const matching
                        let key = match &key_pat.node {
                            ast::Pattern::Literal(lit) => self.const_eval_literal(lit)?,
                            _ => {
                                return Err("map pattern key must be a literal".to_string());
                            }
                        };

                        // Find the value in the map
                        let map_value = map_entries
                            .iter()
                            .find(|(k, _)| k == &key)
                            .map(|(_, v)| v)
                            .ok_or_else(|| "key not found in map constant".to_string())?;

                        self.const_match_pattern(&val_pat.node, map_value, bindings)?;
                    }
                    Ok(())
                } else {
                    Err("expected map in constant pattern".to_string())
                }
            }

            ast::Pattern::Type { type_name, binding } => {
                // Check type matches
                let type_matches = matches!(
                    (type_name.as_ref(), value),
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
                    return Err(format!("type pattern '{}' does not match value", type_name));
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
