// Parser for Rill using chumsky 0.12
//
// This parser directly produces AST nodes from the source text.
// It follows the grammar defined in docs/grammar.abnf.
//
// Note: We use .boxed() extensively to reduce type complexity and
// compilation memory usage. Without boxing, chumsky's deeply nested
// impl Trait types can cause the compiler to run out of memory.

use super::*;
use ast::*;
use chumsky::prelude::*;
use diagnostics::{Diagnostic, DiagnosticCode, Diagnostics};

// ============================================================================
// Type Aliases
// ============================================================================

// Use extra state for better error messages
type Extra<'a> = extra::Err<Rich<'a, char, ast::Span>>;

// Boxed parser type for reduced type complexity
type BoxedParser<'a, T> = Boxed<'a, 'a, &'a str, T, Extra<'a>>;

// ============================================================================
// Keywords
// ============================================================================

const KEYWORDS: &[&str] = &[
    "fn", "import", "as", "const", "let", "with", "if", "else", "match", "while", "loop", "for",
    "in", "return", "break", "continue", "true", "false", "bytes",
];

// ============================================================================
// Lexer Helpers
// ============================================================================

/// Parse whitespace and comments
fn whitespace<'a>() -> BoxedParser<'a, ()> {
    let line_comment = just("//")
        .then(any().and_is(just('\n').not()).repeated())
        .ignored();

    let block_comment = just("/*")
        .then(any().and_is(just("*/").not()).repeated())
        .then(just("*/"))
        .ignored();

    choice((line_comment, block_comment, one_of(" \t\n\r").ignored()))
        .repeated()
        .ignored()
        .boxed()
}

/// Parse a keyword
fn kw<'a>(keyword: &'static str) -> BoxedParser<'a, ()> {
    text::keyword(keyword)
        .ignored()
        .padded_by(whitespace())
        .boxed()
}

/// Parse an identifier
fn ident<'a>() -> BoxedParser<'a, Identifier> {
    text::ident()
        .try_map(|s: &str, span| {
            if KEYWORDS.contains(&s) {
                Err(Rich::custom(span, format!("'{}' is a reserved keyword", s)))
            } else if s == "_" {
                Err(Rich::custom(
                    span,
                    "'_' is not a valid identifier (use in patterns only)",
                ))
            } else {
                Ok(Identifier(s.to_string()))
            }
        })
        .padded_by(whitespace())
        .boxed()
}

/// Parse a qualified name: either `name` or `namespace::name`
/// Returns (namespace, name) where namespace is None for simple names
fn qualified_name<'a>() -> BoxedParser<'a, (Option<Identifier>, Identifier)> {
    ident()
        .then(
            just("::")
                .padded_by(whitespace())
                .ignore_then(ident())
                .or_not(),
        )
        .map(|(first, second)| match second {
            Some(name) => (Some(first), name), // namespace::name
            None => (None, first),             // just name
        })
        .boxed()
}

// ============================================================================
// Literals
// ============================================================================

fn bool_literal<'a>() -> BoxedParser<'a, Literal> {
    kw("true")
        .to(Literal::Bool(true))
        .or(kw("false").to(Literal::Bool(false)))
        .boxed()
}

fn uint_literal<'a>() -> BoxedParser<'a, Literal> {
    let hex = just("0x")
        .or(just("0X"))
        .ignore_then(text::digits(16).to_slice())
        .try_map(|s: &str, span| {
            u64::from_str_radix(s, 16)
                .map(Literal::UInt)
                .map_err(|e| Rich::custom(span, format!("Invalid hex literal: {}", e)))
        });

    let decimal = text::int(10).try_map(|s: &str, span| {
        s.parse::<u64>()
            .map(Literal::UInt)
            .map_err(|e| Rich::custom(span, format!("Invalid integer literal: {}", e)))
    });

    hex.or(decimal).boxed()
}

fn int_literal<'a>() -> BoxedParser<'a, Literal> {
    just('-')
        .ignore_then(text::int(10))
        .to_slice()
        .try_map(|s: &str, span| {
            format!("-{}", s)
                .parse::<i64>()
                .map(Literal::Int)
                .map_err(|e| Rich::custom(span, format!("Invalid integer literal: {}", e)))
        })
        .boxed()
}

fn float_literal<'a>() -> BoxedParser<'a, Literal> {
    let digits = text::digits(10);
    let frac = just('.').then(digits);
    let exp = just('e')
        .or(just('E'))
        .then(just('+').or(just('-')).or_not())
        .then(digits);

    just('-')
        .or_not()
        .then(text::int(10))
        .then(frac)
        .then(exp.or_not())
        .to_slice()
        .try_map(|s: &str, span| {
            s.parse::<f64>()
                .map(Literal::Float)
                .map_err(|e| Rich::custom(span, format!("Invalid float literal: {}", e)))
        })
        .boxed()
}

/// Unicode escape: \u{XXXX} — variable length, 1-6 hex digits, full Unicode range.
/// Examples: \u{41} = 'A', \u{E9} = 'é', \u{1F600} = '😀'
fn unicode_escape<'a>() -> BoxedParser<'a, char> {
    just('u')
        .ignore_then(
            text::digits(16)
                .at_least(1)
                .at_most(6)
                .to_slice()
                .delimited_by(just('{'), just('}'))
                .try_map(|s: &str, span| {
                    u32::from_str_radix(s, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| Rich::custom(span, "invalid unicode escape"))
                }),
        )
        .boxed()
}

fn escape_char<'a>() -> BoxedParser<'a, char> {
    just('\\')
        .ignore_then(choice((
            just('\\').to('\\'),
            just('"').to('"'),
            just('n').to('\n'),
            just('r').to('\r'),
            just('t').to('\t'),
            unicode_escape(),
        )))
        .boxed()
}

fn string_char<'a>() -> BoxedParser<'a, char> {
    none_of("\\\"").or(escape_char()).boxed()
}

/// Character literal: 'A' → UInt(65), '\n' → UInt(10), '\u{E9}' → UInt(233)
/// Syntactic sugar for UInt code points — no Char type.
fn char_literal<'a>() -> BoxedParser<'a, Literal> {
    let char_escape = just('\\')
        .ignore_then(choice((
            just('\\').to('\\'),
            just('\'').to('\''),
            just('n').to('\n'),
            just('r').to('\r'),
            just('t').to('\t'),
            unicode_escape(),
        )))
        .boxed();

    just('\'')
        .ignore_then(none_of("\\'").or(char_escape))
        .then_ignore(just('\''))
        .map(|c| Literal::UInt(c as u64))
        .boxed()
}

fn text_literal<'a>() -> BoxedParser<'a, Literal> {
    just('"')
        .ignore_then(string_char().repeated().collect::<String>())
        .then_ignore(just('"'))
        .map(Literal::Text)
        .boxed()
}

fn string_literal<'a>() -> BoxedParser<'a, String> {
    just('"')
        .ignore_then(string_char().repeated().collect::<String>())
        .then_ignore(just('"'))
        .boxed()
}

fn hex_byte<'a>() -> BoxedParser<'a, u8> {
    just("0x")
        .ignore_then(
            text::digits(16)
                .exactly(2)
                .to_slice()
                .try_map(|s: &str, span| {
                    u8::from_str_radix(s, 16)
                        .map_err(|e| Rich::custom(span, format!("Invalid hex byte: {}", e)))
                }),
        )
        .boxed()
}

fn bytes_literal<'a>() -> BoxedParser<'a, Literal> {
    kw("bytes")
        .ignore_then(just('(').padded_by(whitespace()))
        .ignore_then(just('[').padded_by(whitespace()))
        .ignore_then(
            hex_byte()
                .padded_by(whitespace())
                .separated_by(just(',').padded_by(whitespace()))
                .allow_trailing()
                .collect::<Vec<_>>(),
        )
        .then_ignore(just(']').padded_by(whitespace()))
        .then_ignore(just(')').padded_by(whitespace()))
        .map(Literal::Bytes)
        .boxed()
}

// ============================================================================
// Operators
// ============================================================================

fn assign_op<'a>() -> BoxedParser<'a, AssignmentOp> {
    choice((
        just("<<=").to(AssignmentOp::ShlAssign),
        just(">>=").to(AssignmentOp::ShrAssign),
        just("+=").to(AssignmentOp::AddAssign),
        just("-=").to(AssignmentOp::SubAssign),
        just("*=").to(AssignmentOp::MulAssign),
        just("/=").to(AssignmentOp::DivAssign),
        just("%=").to(AssignmentOp::ModAssign),
        just("&=").to(AssignmentOp::AndAssign),
        just("|=").to(AssignmentOp::OrAssign),
        just("^=").to(AssignmentOp::XorAssign),
        just('=').to(AssignmentOp::Assign),
    ))
    .padded_by(whitespace())
    .boxed()
}

// ============================================================================
// Expressions
// ============================================================================

fn expression<'a>() -> BoxedParser<'a, Expression> {
    recursive(|expr| {
        // Build parsers using the recursive handle
        let expr_boxed: BoxedParser<'a, Expression> = expr.clone().boxed();

        // Primary expressions
        let primary = primary_expr(expr_boxed.clone());

        // Postfix operations
        let postfix = postfix_expr(expr_boxed.clone(), primary);

        // Bit test operator (between postfix and unary)
        let bittest = bittest_expr(postfix);

        // Unary prefix operators
        let unary = unary_expr(bittest);

        // Type cast: expr as Type (between unary and binary)
        let cast = cast_expr(unary);

        // Binary operators with precedence
        let binary = binary_expr(cast);

        // Assignment expression (lowest precedence, right-associative)
        assign_expr(binary, expr_boxed)
    })
    .boxed()
}

fn primary_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    // Array literal
    let array_lit = expr
        .clone()
        .padded_by(whitespace())
        .separated_by(just(',').padded_by(whitespace()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(
            just('[').padded_by(whitespace()),
            just(']').padded_by(whitespace()),
        )
        .map(|exprs| Expression::Literal(Literal::Array(exprs)));

    // Map literal
    let map_entry = expr
        .clone()
        .padded_by(whitespace())
        .then_ignore(just(':').padded_by(whitespace()))
        .then(expr.clone().padded_by(whitespace()));

    let map_lit = map_entry
        .separated_by(just(',').padded_by(whitespace()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(
            just('{').padded_by(whitespace()),
            just('}').padded_by(whitespace()),
        )
        .map(|entries| Expression::Literal(Literal::Map(entries)));

    // Simple literals
    let literal = choice((
        bool_literal(),
        bytes_literal(),
        float_literal(),
        int_literal(),
        uint_literal(),
        char_literal(),
        text_literal(),
    ))
    .padded_by(whitespace())
    .map(Expression::Literal);

    // Function call arguments parser
    let call_args = expr
        .clone()
        .padded_by(whitespace())
        .separated_by(just(',').padded_by(whitespace()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(
            just('(').padded_by(whitespace()),
            just(')').padded_by(whitespace()),
        );

    // Variable, qualified name, or function call
    // Function calls can ONLY follow an identifier or namespace::identifier
    let callable_or_variable =
        qualified_name()
            .then(call_args.or_not())
            .map(|((namespace, name), args)| match args {
                Some(arguments) => Expression::FunctionCall {
                    namespace,
                    name,
                    arguments,
                },
                None => match namespace {
                    Some(ns) => Expression::QualifiedName {
                        namespace: ns,
                        name,
                    },
                    None => Expression::Variable(name),
                },
            });

    let paren = expr.clone().padded_by(whitespace()).delimited_by(
        just('(').padded_by(whitespace()),
        just(')').padded_by(whitespace()),
    );

    // Block expression
    let block = block_expr(expr.clone());

    // Control flow expressions
    let if_e = if_expr(expr.clone());
    let while_e = while_expr(expr.clone());
    let loop_e = loop_expr(expr.clone());
    let for_e = for_expr(expr.clone());
    let match_e = match_expr(expr);

    choice((
        if_e,
        while_e,
        loop_e,
        for_e,
        match_e,
        literal,
        array_lit,
        map_lit,
        block,
        paren,
        callable_or_variable,
    ))
    .boxed()
}

/// Postfix operations: member access and indexing only
/// Function calls are parsed in primary_expr, not as postfix
#[derive(Clone, Debug)]
enum PostfixOp {
    Member(Expression),
    Index(Expression),
}

fn postfix_expr<'a>(
    expr: BoxedParser<'a, Expression>,
    primary: BoxedParser<'a, Expression>,
) -> BoxedParser<'a, Expression> {
    // Member access: obj.field or obj.(expr)
    let member_ident = just('.')
        .padded_by(whitespace())
        .ignore_then(ident())
        .map(|id| PostfixOp::Member(Expression::Literal(Literal::Text(id.0))));

    let member_dyn = just('.')
        .ignore_then(expr.clone().padded_by(whitespace()).delimited_by(
            just('(').padded_by(whitespace()),
            just(')').padded_by(whitespace()),
        ))
        .map(PostfixOp::Member);

    // Array/map indexing: arr[i]
    let index = expr
        .padded_by(whitespace())
        .delimited_by(
            just('[').padded_by(whitespace()),
            just(']').padded_by(whitespace()),
        )
        .map(PostfixOp::Index);

    // No Call here - function calls are only valid after identifiers,
    // and are parsed in primary_expr via callable_or_variable
    let postfix_op = choice((member_ident, member_dyn, index)).boxed();

    primary
        .foldl(postfix_op.repeated(), |lhs, op| match op {
            PostfixOp::Member(key) => Expression::MemberAccess {
                object: Box::new(lhs),
                member: Box::new(key),
            },
            PostfixOp::Index(idx) => Expression::ArrayAccess {
                array: Box::new(lhs),
                index: Box::new(idx),
            },
        })
        .boxed()
}

fn bittest_expr<'a>(postfix: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    // Bit test operator: X @ B - returns true if bit B is set in X
    let bittest_op = just('@')
        .to(BinaryOperator::BitTest)
        .padded_by(whitespace())
        .boxed();

    postfix
        .clone()
        .foldl(bittest_op.then(postfix).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed()
}

fn unary_expr<'a>(bittest: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    let unary_op = choice((
        just('!').to(UnaryOperator::Not),
        just('-').to(UnaryOperator::Negate),
        just('~').to(UnaryOperator::BitwiseNot),
    ))
    .padded_by(whitespace())
    .boxed();

    unary_op
        .repeated()
        .foldr(bittest, |op, rhs| Expression::UnaryOp {
            op,
            operand: Box::new(rhs),
        })
        .boxed()
}

fn cast_expr<'a>(unary: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    // Type cast: expr as Type
    // Uses text::keyword to match "as" followed by a word boundary,
    // then parses a type name identifier.
    let cast_target = text::keyword("as")
        .padded_by(whitespace())
        .ignore_then(ident());

    unary
        .foldl(cast_target.repeated(), |lhs, target_type| {
            Expression::Cast {
                value: Box::new(lhs),
                target_type,
            }
        })
        .boxed()
}

fn binary_expr<'a>(atom: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    // Multiplicative
    let mult_op = choice((
        just('*').to(BinaryOperator::Multiply),
        just('/').to(BinaryOperator::Divide),
        just('%').to(BinaryOperator::Modulo),
    ))
    .padded_by(whitespace())
    .boxed();

    let multiplicative = atom
        .clone()
        .foldl(mult_op.then(atom).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Additive
    let add_op = choice((
        just('+').to(BinaryOperator::Add),
        just('-').to(BinaryOperator::Subtract),
    ))
    .padded_by(whitespace())
    .boxed();

    let additive = multiplicative
        .clone()
        .foldl(add_op.then(multiplicative).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Shift
    let shift_op = choice((
        just("<<").to(BinaryOperator::ShiftLeft),
        just(">>").to(BinaryOperator::ShiftRight),
    ))
    .padded_by(whitespace())
    .boxed();

    let shift = additive
        .clone()
        .foldl(shift_op.then(additive).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Bitwise AND
    let bitand_op = just('&')
        .then_ignore(just('&').not())
        .to(BinaryOperator::BitwiseAnd)
        .padded_by(whitespace())
        .boxed();

    let bitand = shift
        .clone()
        .foldl(bitand_op.then(shift).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Bitwise XOR
    let bitxor_op = just('^')
        .to(BinaryOperator::BitwiseXor)
        .padded_by(whitespace())
        .boxed();

    let bitxor = bitand
        .clone()
        .foldl(bitxor_op.then(bitand).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Bitwise OR
    let bitor_op = just('|')
        .then_ignore(just('|').not())
        .to(BinaryOperator::BitwiseOr)
        .padded_by(whitespace())
        .boxed();

    let bitor = bitxor
        .clone()
        .foldl(bitor_op.then(bitxor).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Comparison
    let cmp_op = choice((
        just("<=").to(BinaryOperator::LessEqual),
        just(">=").to(BinaryOperator::GreaterEqual),
        just('<').to(BinaryOperator::Less),
        just('>').to(BinaryOperator::Greater),
    ))
    .padded_by(whitespace())
    .boxed();

    let comparison = bitor
        .clone()
        .foldl(cmp_op.then(bitor).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Equality
    let eq_op = choice((
        just("==").to(BinaryOperator::Equal),
        just("!=").to(BinaryOperator::NotEqual),
    ))
    .padded_by(whitespace())
    .boxed();

    let equality = comparison
        .clone()
        .foldl(eq_op.then(comparison).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Logical AND
    let and_op = just("&&")
        .to(BinaryOperator::And)
        .padded_by(whitespace())
        .boxed();

    let logical_and = equality
        .clone()
        .foldl(and_op.then(equality).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Logical OR
    let or_op = just("||")
        .to(BinaryOperator::Or)
        .padded_by(whitespace())
        .boxed();

    let logical_or = logical_and
        .clone()
        .foldl(or_op.then(logical_and).repeated(), |l, (op, r)| {
            Expression::BinaryOp {
                left: Box::new(l),
                op,
                right: Box::new(r),
            }
        })
        .boxed();

    // Range (lowest precedence)
    // 0..10 or 0..=10
    // Produces an Array when evaluated
    let range_op = just("..")
        .then(just('=').or_not())
        .padded_by(whitespace())
        .boxed();

    logical_or
        .clone()
        .then(range_op.then(logical_or).or_not())
        .map(|(start, rest)| match rest {
            Some(((_, inclusive), end)) => Expression::Range {
                start: Box::new(start),
                end: Box::new(end),
                inclusive: inclusive.is_some(),
            },
            None => start,
        })
        .boxed()
}

/// Assignment expression parser
/// Assignment has lowest precedence and is right-associative
/// a = b = c parses as a = (b = c)
fn assign_expr<'a>(
    binary: BoxedParser<'a, Expression>,
    expr: BoxedParser<'a, Expression>,
) -> BoxedParser<'a, Expression> {
    // Right-associative: use foldr pattern
    // First, parse the left operand (binary expression)
    // Then optionally parse assign-op followed by another assign-expr
    binary
        .clone()
        .then(assign_op().then(expr).or_not())
        .map(|(target, rest)| match rest {
            Some((op, value)) => Expression::Assignment {
                target: Box::new(target),
                op,
                value: Box::new(value),
            },
            None => target,
        })
        .boxed()
}

// ============================================================================
// Control Flow
// ============================================================================

/// A block item — either a statement (with `;`) or a trailing expression (without `;`).
/// Used internally by `block_body` to resolve the statement-vs-final-expression ambiguity.
enum BlockItem {
    Statement(Stmt),
    TrailingExpr(Expression),
}

/// Parse a block body: { items* }
/// Returns (statements, optional_final_expression)
///
/// Each item is either a statement (with `;`) or an expression without `;`.
/// The LAST trailing expression becomes the block's return value.
/// Earlier trailing expressions become void statements (e.g., `if cond { }` mid-block).
/// Non-block expressions without `;` at the end are also final expressions (`42`).
fn block_body<'a>(
    expr: BoxedParser<'a, Expression>,
) -> BoxedParser<'a, (Vec<Stmt>, Option<Expression>)> {
    // A statement (requires `;` for non-keyword forms)
    let stmt_item = statement(expr.clone()).map(BlockItem::Statement).boxed();

    // A trailing expression (no `;`) — could be final expr or void control flow
    let trailing_item = expr
        .padded_by(whitespace())
        .map(BlockItem::TrailingExpr)
        .boxed();

    // Try statement first (with `;`), fall back to trailing expression
    choice((stmt_item, trailing_item))
        .repeated()
        .collect::<Vec<_>>()
        .map(|items| {
            let mut statements = Vec::new();
            let mut final_expr = None;
            let len = items.len();

            for (i, item) in items.into_iter().enumerate() {
                let is_last = i == len - 1;
                match item {
                    BlockItem::Statement(stmt) => statements.push(stmt),
                    BlockItem::TrailingExpr(expr) => {
                        if is_last {
                            final_expr = Some(expr);
                        } else {
                            // Mid-block expression without ; → void statement
                            let span = chumsky::span::Span::new((), 0..0);
                            statements.push(ast::Spanned::new(Statement::Expression(expr), span));
                        }
                    }
                }
            }

            (statements, final_expr)
        })
        .delimited_by(
            just('{').padded_by(whitespace()),
            just('}').padded_by(whitespace()),
        )
        .boxed()
}

fn block_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    block_body(expr)
        .map(|(statements, final_expr)| Expression::Block {
            statements,
            final_expr: final_expr.map(Box::new),
        })
        .boxed()
}

fn if_condition<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, IfCondition> {
    // if let pattern = expr - by-value binding, body runs if pattern matches
    let let_binding = kw("let")
        .ignore_then(pattern())
        .then_ignore(just('=').padded_by(whitespace()))
        .then(expr.clone().padded_by(whitespace()))
        .map(|(pattern, value)| IfCondition::Let { pattern, value });

    // if with pattern = expr - by-reference binding, body runs if pattern matches
    let with_binding = kw("with")
        .ignore_then(pattern())
        .then_ignore(just('=').padded_by(whitespace()))
        .then(expr.clone().padded_by(whitespace()))
        .map(|(pattern, value)| IfCondition::With { pattern, value });

    let bool_cond = expr.map(IfCondition::Bool);

    choice((let_binding, with_binding, bool_cond)).boxed()
}

fn if_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    recursive(|if_e| {
        let body = block_body(expr.clone());

        // else if chain returns the If expression directly
        // else block returns (statements, final_expr)
        let else_branch = kw("else").ignore_then(
            if_e.map_with(|e, extra| {
                // else if: wrap in a spanned statement, no final expr
                let stmt = ast::Spanned::new(Statement::Expression(e), extra.span());
                (vec![stmt], None)
            })
            .or(block_body(expr.clone())),
        );

        kw("if")
            .ignore_then(
                if_condition(expr.clone())
                    .separated_by(just("&&").padded_by(whitespace()))
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then(body)
            .then(else_branch.or_not())
            .map(|((conditions, (then_block, then_expr)), else_branch)| {
                let (else_block, else_expr) = match else_branch {
                    Some((stmts, expr)) => (Some(stmts), expr),
                    None => (None, None),
                };
                Expression::If {
                    conditions,
                    then_block,
                    then_expr: then_expr.map(Box::new),
                    else_block,
                    else_expr: else_expr.map(Box::new),
                }
            })
    })
    .boxed()
}

fn while_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    kw("while")
        .ignore_then(expr.clone().padded_by(whitespace()))
        .then(block_body(expr))
        .map(|(condition, (body, body_expr))| Expression::While {
            condition: Box::new(condition),
            body,
            body_expr: body_expr.map(Box::new),
        })
        .boxed()
}

fn loop_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    kw("loop")
        .ignore_then(block_body(expr))
        .map(|(body, body_expr)| Expression::Loop {
            body,
            body_expr: body_expr.map(Box::new),
        })
        .boxed()
}

fn for_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    // Optional `let` or `with` keyword for binding mode
    // `let` = by-value (copy), `with` or default = by-reference
    let binding_is_value = choice((
        kw("let").to(true),   // by-value
        kw("with").to(false), // by-reference (explicit)
    ))
    .or_not()
    .map(|opt| opt.unwrap_or(false)) // default = by-reference
    .boxed();

    // Pair binding: for k, v in map { }
    // Must try pair first (ident "," ident) before single
    let pair_binding = ident()
        .then_ignore(just(',').padded_by(whitespace()))
        .then(ident())
        .map(|(first, second)| ForBinding::Pair(first, second))
        .boxed();

    // Single variable binding: for x in arr { }
    let single_var = ident().map(ForBinding::Single).boxed();

    let binding = choice((pair_binding, single_var)).boxed();

    // Iterable is any expression (Array, Map, or Range)
    // Range expressions (0..10, 0..=10) are parsed as regular expressions
    kw("for")
        .ignore_then(binding_is_value)
        .then(binding)
        .then_ignore(kw("in"))
        .then(expr.clone().padded_by(whitespace()))
        .then(block_body(expr))
        .map(
            |(((binding_is_value, binding), iterable), (body, body_expr))| Expression::For {
                binding_is_value,
                binding,
                iterable: Box::new(iterable),
                body,
                body_expr: body_expr.map(Box::new),
            },
        )
        .boxed()
}

fn pattern<'a>() -> BoxedParser<'a, Pat> {
    pattern_inner().boxed()
}

fn pattern_inner<'a>() -> BoxedParser<'a, Pat> {
    recursive(|pat| {
        // Helper to wrap a Pattern with span
        let spanned = |p: BoxedParser<'a, Pattern>| -> BoxedParser<'a, Pat> {
            p.map_with(|node, extra| ast::Spanned::new(node, extra.span()))
                .boxed()
        };

        let wildcard = spanned(
            just('_')
                .then_ignore(text::ident().not().rewind())
                .to(Pattern::Wildcard)
                .padded_by(whitespace())
                .boxed(),
        );

        let bool_pat = spanned(
            kw("true")
                .to(Pattern::Literal(Literal::Bool(true)))
                .or(kw("false").to(Pattern::Literal(Literal::Bool(false))))
                .boxed(),
        );

        let uint_pat = spanned(
            text::int(10)
                .try_map(|s: &str, span| {
                    s.parse::<u64>()
                        .map(|n| Pattern::Literal(Literal::UInt(n)))
                        .map_err(|e| Rich::custom(span, format!("Invalid integer: {}", e)))
                })
                .padded_by(whitespace())
                .boxed(),
        );

        let text_pat = spanned(
            just('"')
                .ignore_then(string_char().repeated().collect::<String>())
                .then_ignore(just('"'))
                .map(|s| Pattern::Literal(Literal::Text(s)))
                .padded_by(whitespace())
                .boxed(),
        );

        let pat_boxed: BoxedParser<'a, Pat> = pat.clone().boxed();

        // Type pattern with optional binding: UInt, UInt(x), Array([a, b])
        // Type names start with uppercase
        // Valid type names for type patterns
        const TYPE_NAMES: &[&str] = &[
            "Bool", "UInt", "Int", "Float", "Text", "Bytes", "Array", "Map",
        ];

        let type_pattern = ident()
            .then(
                pat_boxed
                    .clone()
                    .padded_by(whitespace())
                    .delimited_by(
                        just('(').padded_by(whitespace()),
                        just(')').padded_by(whitespace()),
                    )
                    .or_not(),
            )
            .try_map(|(id, binding), span| {
                let is_type_name = TYPE_NAMES.contains(&id.0.as_str());
                if is_type_name {
                    Ok(Pattern::Type {
                        type_name: id,
                        binding: binding.map(Box::new),
                    })
                } else if binding.is_some() {
                    // Non-type identifier with parentheses is an error
                    Err(Rich::custom(
                        span,
                        format!(
                            "'{}' is not a type name (valid types: Bool, UInt, Int, Float, Text, Bytes, Array, Map)",
                            id.0
                        ),
                    ))
                } else {
                    Ok(Pattern::Variable(id))
                }
            })
            .map_with(|node, extra| ast::Spanned::new(node, extra.span()))
            .boxed();

        // Rest capture: ..identifier or just .. (with optional whitespace)
        // .. without identifier means "ignore rest"
        let rest_capture = just("..")
            .padded_by(whitespace())
            .ignore_then(ident().or_not())
            .boxed();

        // Pattern element: either a rest capture or a spanned pattern
        #[derive(Clone)]
        enum PatternElement {
            Pattern(Pat),
            Rest(Option<Identifier>), // None means ".." without variable
        }

        let pattern_element = rest_capture
            .clone()
            .map(PatternElement::Rest)
            .or(pat_boxed.clone().map(PatternElement::Pattern))
            .padded_by(whitespace())
            .boxed();

        // Array pattern: [a, b, c] or [a, ..rest] or [a, ..rest, b]
        let array_pat = pattern_element
            .clone()
            .separated_by(just(',').padded_by(whitespace()))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(
                just('[').padded_by(whitespace()),
                just(']').padded_by(whitespace()),
            )
            .map(|elements| {
                // Find if there's a rest element and split around it
                let mut before = Vec::new();
                let mut has_rest = false;
                let mut rest: Option<Identifier> = None;
                let mut after = Vec::new();

                for elem in elements {
                    match elem {
                        PatternElement::Pattern(p) => {
                            if has_rest {
                                after.push(p);
                            } else {
                                before.push(p);
                            }
                        }
                        PatternElement::Rest(maybe_id) => {
                            has_rest = true;
                            rest = maybe_id;
                        }
                    }
                }

                if has_rest {
                    Pattern::ArrayRest {
                        before,
                        rest,
                        after,
                    }
                } else {
                    Pattern::Array(before)
                }
            })
            .map_with(|node, extra| ast::Spanned::new(node, extra.span()))
            .boxed();

        let map_entry = pat_boxed
            .clone()
            .padded_by(whitespace())
            .then_ignore(just(':').padded_by(whitespace()))
            .then(pat_boxed.padded_by(whitespace()));

        let map_pat = map_entry
            .separated_by(just(',').padded_by(whitespace()))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(
                just('{').padded_by(whitespace()),
                just('}').padded_by(whitespace()),
            )
            .map(Pattern::Map)
            .map_with(|node, extra| ast::Spanned::new(node, extra.span()))
            .boxed();

        choice((
            wildcard,
            bool_pat,
            uint_pat,
            text_pat,
            array_pat,
            map_pat,
            type_pattern,
        ))
    })
    .boxed()
}

fn match_arm<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, MatchArm> {
    // Optional `let` or `with` prefix for binding mode
    // `let` = by-value (copy), `with` or default = by-reference
    let binding_is_value = choice((
        kw("let").to(true),   // by-value
        kw("with").to(false), // by-reference (explicit)
    ))
    .or_not()
    .map(|opt| opt.unwrap_or(false)) // default = by-reference
    .boxed();

    let guard = kw("if").ignore_then(expr.clone().padded_by(whitespace()));

    let body = just("=>")
        .padded_by(whitespace())
        .ignore_then(block_body(expr))
        .boxed();

    binding_is_value
        .then(pattern())
        .then(guard.or_not())
        .then(body)
        .then_ignore(just(',').padded_by(whitespace()).or_not())
        .map(
            |(((binding_is_value, pattern), guard), (body, body_expr))| MatchArm {
                binding_is_value,
                pattern,
                guard,
                body,
                body_expr: body_expr.map(Box::new),
            },
        )
        .boxed()
}

fn match_expr<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Expression> {
    kw("match")
        .ignore_then(expr.clone().padded_by(whitespace()))
        .then(match_arm(expr).repeated().collect::<Vec<_>>().delimited_by(
            just('{').padded_by(whitespace()),
            just('}').padded_by(whitespace()),
        ))
        .map(|(value, arms)| Expression::Match {
            value: Box::new(value),
            arms,
        })
        .boxed()
}

// ============================================================================
// Statements
// ============================================================================

fn statement<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Stmt> {
    // Variable declaration with pattern: let x = expr; or let [a, b] = expr;
    let var_decl = kw("let")
        .ignore_then(pattern())
        .then_ignore(just('=').padded_by(whitespace()))
        .then(expr.clone().padded_by(whitespace()))
        .then_ignore(just(';').padded_by(whitespace()))
        .map(|(pattern, initializer)| Statement::VarDecl {
            pattern,
            initializer,
        })
        .boxed();

    let with_stmt = with_statement(expr.clone());

    choice((var_decl, with_stmt, statement_inner(expr)))
        .map_with(|stmt, extra| ast::Spanned::new(stmt, extra.span()))
        .boxed()
}

fn with_statement<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Statement> {
    // Simple statement form: with pattern = expr;
    // Creates by-reference bindings (variables may be missing if pattern doesn't match)
    kw("with")
        .ignore_then(pattern())
        .then_ignore(just('=').padded_by(whitespace()))
        .then(expr.padded_by(whitespace()))
        .then_ignore(just(';').padded_by(whitespace()))
        .map(|(pattern, value)| Statement::With { pattern, value })
        .boxed()
}

fn statement_inner<'a>(expr: BoxedParser<'a, Expression>) -> BoxedParser<'a, Statement> {
    let return_stmt = kw("return")
        .ignore_then(expr.clone().padded_by(whitespace()).or_not())
        .then_ignore(just(';').padded_by(whitespace()))
        .map(|value| Statement::Return { value })
        .boxed();

    let break_stmt = kw("break")
        .ignore_then(expr.clone().padded_by(whitespace()).or_not())
        .then_ignore(just(';').padded_by(whitespace()))
        .map(|value| Statement::Break { value })
        .boxed();

    let continue_stmt = kw("continue")
        .ignore_then(just(';').padded_by(whitespace()))
        .to(Statement::Continue)
        .boxed();

    // Note: Assignment is now an expression, not a separate statement form
    // x = y; is parsed as expression-stmt where the expression is an assignment

    // All expression statements require a trailing semicolon.
    // Control flow used as a statement: `if cond { ... };` or `while cond { ... };`
    // Control flow as final expression (no ;): handled by block_body's final_expr parser.
    let expression_stmt = expr
        .padded_by(whitespace())
        .then_ignore(just(';').padded_by(whitespace()))
        .map(Statement::Expression)
        .boxed();

    choice((return_stmt, break_stmt, continue_stmt, expression_stmt)).boxed()
}

// ============================================================================
// Top-level
// ============================================================================

fn import_path<'a>() -> BoxedParser<'a, ImportPath> {
    let file_path = string_literal().map(ImportPath::File);

    let stdlib_path = ident()
        .separated_by(just('.'))
        .at_least(1)
        .collect::<Vec<_>>()
        .map(ImportPath::Stdlib);

    file_path.or(stdlib_path).padded_by(whitespace()).boxed()
}

fn import<'a>() -> BoxedParser<'a, ast::Spanned<Import>> {
    let alias = kw("as").ignore_then(ident());

    kw("import")
        .ignore_then(import_path())
        .then(alias.or_not())
        .then_ignore(just(';').padded_by(whitespace()))
        .map(|(path, alias)| Import { path, alias })
        .map_with(|imp, extra| ast::Spanned::new(imp, extra.span()))
        .boxed()
}

fn constant<'a>() -> BoxedParser<'a, ast::Spanned<Constant>> {
    // const pattern = expression;
    // Compiler validates const-evaluability; pattern match failure is compile error
    kw("const")
        .ignore_then(pattern())
        .then_ignore(just('=').padded_by(whitespace()))
        .then(expression())
        .then_ignore(just(';').padded_by(whitespace()))
        .map(|(pattern, value)| Constant { pattern, value })
        .map_with(|c, extra| ast::Spanned::new(c, extra.span()))
        .boxed()
}

/// Parse binding mode prefix: `let` (by-value) or `with` (by-reference)
fn binding_mode<'a>() -> BoxedParser<'a, Option<bool>> {
    choice((
        kw("let").to(true),   // by-value
        kw("with").to(false), // by-reference (explicit)
    ))
    .or_not()
    .boxed()
}

/// Parse a single function parameter: ["let" / "with"] identifier
/// `let` = by-value, `with` or default = by-reference
fn function_param<'a>() -> BoxedParser<'a, ast::FunctionParam> {
    binding_mode()
        .then(ident())
        .map(|(binding_mode, name)| ast::FunctionParam {
            name,
            is_value: binding_mode.unwrap_or(false), // default = by-reference
        })
        .boxed()
}

/// Parse a rest parameter: ["let" / "with"] ".." identifier
/// Captures excess arguments as an Array
fn rest_param<'a>() -> BoxedParser<'a, ast::FunctionParam> {
    binding_mode()
        .then_ignore(just('.').then(just('.')).padded_by(whitespace()))
        .then(ident())
        .map(|(binding_mode, name)| ast::FunctionParam {
            name,
            is_value: binding_mode.unwrap_or(false), // default = by-reference
        })
        .boxed()
}

/// Parse function parameter list: regular params optionally followed by rest param
/// Returns (regular_params, rest_param)
fn param_list<'a>() -> BoxedParser<'a, (Vec<ast::FunctionParam>, Option<ast::FunctionParam>)> {
    // Regular params separated by commas
    let regular_params = function_param()
        .separated_by(just(',').padded_by(whitespace()))
        .collect::<Vec<_>>();

    // Optional rest param (with leading comma if there are regular params)
    let comma = just(',').padded_by(whitespace());

    // Case 1: regular params, optionally followed by comma and rest
    let with_regular = regular_params
        .then(comma.ignore_then(rest_param()).or_not())
        .map(|(params, rest)| (params, rest));

    // Case 2: just a rest param (no regular params)
    let just_rest = rest_param().map(|rest| (vec![], Some(rest)));

    // Try just_rest first to avoid ambiguity, then with_regular
    just_rest.or(with_regular).boxed()
}

// ============================================================================
// Functions
// ============================================================================

fn function<'a>() -> BoxedParser<'a, ast::Spanned<Function>> {
    kw("fn")
        .ignore_then(ident())
        .then(param_list().delimited_by(
            just('(').padded_by(whitespace()),
            just(')').padded_by(whitespace()),
        ))
        .then(block_body(expression()))
        .map(
            |((name, (params, rest_param)), (statements, final_expr))| Function {
                name,
                params,
                rest_param,
                statements,
                final_expr: final_expr.map(Box::new),
            },
        )
        .map_with(|f, extra| ast::Spanned::new(f, extra.span()))
        .boxed()
}

// ============================================================================
// Program
// ============================================================================

#[derive(Clone)]
enum TopLevel {
    Import(ast::Spanned<Import>),
    Constant(ast::Spanned<Constant>),
    Function(ast::Spanned<Function>),
}

fn program<'a>() -> BoxedParser<'a, AstProgram> {
    let top_level = choice((
        import().map(TopLevel::Import),
        constant().map(TopLevel::Constant),
        function().map(TopLevel::Function),
    ))
    .boxed();

    whitespace()
        .ignore_then(top_level.repeated().collect::<Vec<_>>())
        .then_ignore(end())
        .map(|items| {
            let mut imports: Vec<ast::Spanned<Import>> = Vec::new();
            let mut constants: Vec<ast::Spanned<Constant>> = Vec::new();
            let mut functions: Vec<ast::Spanned<Function>> = Vec::new();

            for item in items {
                match item {
                    TopLevel::Import(i) => imports.push(i),
                    TopLevel::Constant(c) => constants.push(c),
                    TopLevel::Function(f) => functions.push(f),
                }
            }

            AstProgram {
                imports,
                constants,
                functions,
            }
        })
        .boxed()
}

// ============================================================================
// Error Conversion
// ============================================================================

/// Convert a chumsky Rich error to a Diagnostic
fn rich_to_diagnostic(error: &Rich<'_, char, Span>) -> Diagnostic {
    let span = *error.span();
    let message = format_rich_error(error);

    // Determine the appropriate error code based on the error
    let code = categorize_parse_error(error);

    let mut diag = Diagnostic::at(code, span, message);

    // Add context from the error's context stack
    // Note: contexts() returns (RichPattern, &str) - we include the label but not the pattern span
    for (_pattern, label) in error.contexts() {
        diag.help(format!("in {}", label));
    }

    diag
}

/// Format a Rich error into a human-readable message
fn format_rich_error(error: &Rich<'_, char, Span>) -> String {
    use chumsky::error::RichReason;

    match error.reason() {
        RichReason::ExpectedFound { expected, found } => {
            let expected_str = if expected.is_empty() {
                "something else".to_string()
            } else {
                let items: Vec<_> = expected.iter().map(|e| format!("{}", e)).collect();
                if items.len() == 1 {
                    items[0].clone()
                } else {
                    format!(
                        "{} or {}",
                        items[..items.len() - 1].join(", "),
                        items.last().unwrap()
                    )
                }
            };

            match found {
                Some(c) => format!("expected {}, found {:?}", expected_str, c),
                None => format!("expected {}, found end of input", expected_str),
            }
        }
        RichReason::Custom(msg) => msg.to_string(),
    }
}

/// Categorize a parse error to determine the diagnostic code
fn categorize_parse_error(error: &Rich<'_, char, Span>) -> DiagnosticCode {
    use chumsky::error::RichReason;

    match error.reason() {
        RichReason::Custom(msg) => {
            if msg.contains("unclosed") || msg.contains("delimiter") {
                DiagnosticCode::E002_UnclosedDelimiter
            } else if msg.contains("escape") {
                DiagnosticCode::E004_InvalidEscape
            } else if msg.contains("literal") || msg.contains("number") {
                DiagnosticCode::E003_InvalidLiteral
            } else {
                DiagnosticCode::E001_UnexpectedToken
            }
        }
        _ => DiagnosticCode::E001_UnexpectedToken,
    }
}

/// Convert multiple Rich errors to Diagnostics
fn convert_parse_errors(errors: Vec<Rich<'_, char, Span>>, diags: &mut Diagnostics) {
    for error in &errors {
        diags.emit(rich_to_diagnostic(error));
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Parse a Rill source file, emitting diagnostics on error
///
/// Returns `Some(AstProgram)` if parsing succeeded, `None` if there were errors.
/// Errors are emitted to the provided diagnostics accumulator.
pub fn parse(input: &str, diags: &mut Diagnostics) -> Option<AstProgram> {
    match program().parse(input).into_result() {
        Ok(program) => Some(program),
        Err(errors) => {
            convert_parse_errors(errors, diags);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a single expression (test helper)
    fn parse_expression(input: &str, diags: &mut Diagnostics) -> Option<Expression> {
        let result = whitespace()
            .ignore_then(expression())
            .then_ignore(whitespace())
            .then_ignore(end())
            .parse(input)
            .into_result();

        match result {
            Ok(expr) => Some(expr),
            Err(errors) => {
                convert_parse_errors(errors, diags);
                None
            }
        }
    }

    // Test helper: parse expression and return Result for easy assertion
    fn try_parse_expr(input: &str) -> Result<Expression, ()> {
        let mut diags = Diagnostics::new();
        parse_expression(input, &mut diags).ok_or(())
    }

    // Test helper: parse program and return Result for easy assertion
    fn try_parse(input: &str) -> Result<AstProgram, ()> {
        let mut diags = Diagnostics::new();
        parse(input, &mut diags).ok_or(())
    }

    #[test]
    fn test_parse_literals() {
        assert!(try_parse_expr("42").is_ok());
        assert!(try_parse_expr("-123").is_ok());
        assert!(try_parse_expr("3.14").is_ok());
        assert!(try_parse_expr("true").is_ok());
        assert!(try_parse_expr("false").is_ok());
        assert!(try_parse_expr("\"hello\"").is_ok());
        assert!(try_parse_expr("[1, 2, 3]").is_ok());
        assert!(try_parse_expr("{1: \"a\", 2: \"b\"}").is_ok());
    }

    #[test]
    fn test_parse_char_literals() {
        // Basic char literals → UInt
        assert!(try_parse_expr("'A'").is_ok());
        assert!(try_parse_expr("'z'").is_ok());
        assert!(try_parse_expr("'0'").is_ok());

        // Escape sequences
        assert!(try_parse_expr("'\\n'").is_ok());
        assert!(try_parse_expr("'\\t'").is_ok());
        assert!(try_parse_expr("'\\''").is_ok());
        assert!(try_parse_expr("'\\\\'").is_ok());

        // Unicode escape
        assert!(try_parse_expr("'\\u{E9}'").is_ok());

        // Verify it produces UInt
        let result = try_parse_expr("'A'").unwrap();
        match result {
            Expression::Literal(Literal::UInt(n)) => assert_eq!(n, 65),
            _ => panic!("Expected UInt literal, got {:?}", result),
        }

        // Verify newline escape
        let result = try_parse_expr("'\\n'").unwrap();
        match result {
            Expression::Literal(Literal::UInt(n)) => assert_eq!(n, 10),
            _ => panic!("Expected UInt literal"),
        }
    }

    #[test]
    fn test_parse_operators() {
        assert!(try_parse_expr("1 + 2").is_ok());
        assert!(try_parse_expr("1 + 2 * 3").is_ok());
        assert!(try_parse_expr("(1 + 2) * 3").is_ok());
        assert!(try_parse_expr("a && b || c").is_ok());
        assert!(try_parse_expr("a == b").is_ok());
        assert!(try_parse_expr("!x").is_ok());
        assert!(try_parse_expr("-x").is_ok());
    }

    #[test]
    fn test_parse_cast() {
        // Basic cast expressions
        assert!(try_parse_expr("x as UInt").is_ok());
        assert!(try_parse_expr("x as Int").is_ok());
        assert!(try_parse_expr("x as Float").is_ok());

        // Cast with sub-expressions
        assert!(try_parse_expr("-1 as UInt").is_ok());
        assert!(try_parse_expr("(a + b) as Float").is_ok());
        assert!(try_parse_expr("arr[0] as Int").is_ok());

        // Chained casts
        assert!(try_parse_expr("x as Int as UInt").is_ok());

        // Cast in larger expressions
        assert!(try_parse_expr("x as Float + 1.0").is_ok());
        assert!(try_parse_expr("x + y as Float").is_ok());
    }

    #[test]
    fn test_parse_cast_ast_structure() {
        // Verify AST node is correct
        let result = try_parse_expr("x as Int").unwrap();
        match result {
            Expression::Cast { value, target_type } => {
                assert!(matches!(*value, Expression::Variable(_)));
                assert_eq!(target_type, Identifier("Int".to_string()));
            }
            _ => panic!("Expected Cast expression"),
        }

        // Verify precedence: x + y as Float → x + (y as Float)
        let result = try_parse_expr("x + y as Float").unwrap();
        match result {
            Expression::BinaryOp { left, op, right } => {
                assert!(matches!(*left, Expression::Variable(_)));
                assert!(matches!(op, BinaryOperator::Add));
                assert!(matches!(*right, Expression::Cast { .. }));
            }
            _ => panic!("Expected BinaryOp with Cast on right"),
        }

        // Verify precedence: -x as UInt → (-x) as UInt
        let result = try_parse_expr("-x as UInt").unwrap();
        match result {
            Expression::Cast { value, target_type } => {
                assert!(matches!(*value, Expression::UnaryOp { .. }));
                assert_eq!(target_type, Identifier("UInt".to_string()));
            }
            _ => panic!("Expected Cast wrapping UnaryOp"),
        }
    }

    #[test]
    fn test_parse_postfix() {
        assert!(try_parse_expr("arr[0]").is_ok());
        assert!(try_parse_expr("obj.field").is_ok());
        assert!(try_parse_expr("func()").is_ok());
        assert!(try_parse_expr("func(a, b)").is_ok());
        assert!(try_parse_expr("arr[0].field").is_ok());
    }

    #[test]
    fn test_parse_namespaced_calls() {
        // Namespaced function calls use :: separator
        assert!(try_parse_expr("bpsec::validate()").is_ok());
        assert!(try_parse_expr("bpsec::validate(bundle)").is_ok());
        assert!(try_parse_expr("cbor::decode(data, schema)").is_ok());

        // Verify it produces the right AST structure
        let result = try_parse_expr("bpsec::validate(bundle)").unwrap();
        match result {
            Expression::FunctionCall {
                namespace,
                name,
                arguments,
            } => {
                assert_eq!(namespace, Some(Identifier("bpsec".to_string())));
                assert_eq!(name, Identifier("validate".to_string()));
                assert_eq!(arguments.len(), 1);
            }
            _ => panic!("Expected FunctionCall"),
        }

        // Simple call should have no namespace
        let result = try_parse_expr("foo()").unwrap();
        match result {
            Expression::FunctionCall { namespace, .. } => {
                assert_eq!(namespace, None);
            }
            _ => panic!("Expected FunctionCall"),
        }

        // Qualified name as expression (for constants)
        let result = try_parse_expr("bpsec::MAX_TTL").unwrap();
        match result {
            Expression::QualifiedName { namespace, name } => {
                assert_eq!(namespace, Identifier("bpsec".to_string()));
                assert_eq!(name, Identifier("MAX_TTL".to_string()));
            }
            _ => panic!("Expected QualifiedName"),
        }

        // obj.field() should fail to parse - () is only valid after identifiers
        // This language doesn't have first-class functions
        // Note: parse_expression may succeed but leave () unparsed, so test in full program
        let program = r#"
            fn test() {
                obj.field();
            }
        "#;
        assert!(
            try_parse(program).is_err(),
            "obj.field() should be a parse error - cannot call field access result"
        );

        // But obj.field (without call) is fine
        assert!(try_parse_expr("obj.field").is_ok());

        // And func().field is fine (accessing field on function result)
        assert!(try_parse_expr("func().field").is_ok());
    }

    #[test]
    fn test_parse_if() {
        assert!(try_parse_expr("if x { 1 }").is_ok());
        assert!(try_parse_expr("if x { 1 } else { 2 }").is_ok());
        // Note: No ? needed in if let - the implicit presence check IS the point
        assert!(try_parse_expr("if let y = x { y }").is_ok());
        assert!(try_parse_expr("if x && let y = z { y }").is_ok());
    }

    #[test]
    fn test_parse_loops() {
        assert!(try_parse_expr("while x { }").is_ok());
        assert!(try_parse_expr("loop { }").is_ok());
        assert!(try_parse_expr("for i in 0..10 { }").is_ok());
        // Reference binding (default)
        assert!(try_parse_expr("for item in array { }").is_ok());
        // Value binding (explicit let)
        assert!(try_parse_expr("for let item in array { }").is_ok());
        assert!(try_parse_expr("for let i in 0..10 { }").is_ok());
        // Pair binding for maps
        assert!(try_parse_expr("for k, v in map { }").is_ok());
        assert!(try_parse_expr("for let k, v in map { }").is_ok());
        // Pair binding for arrays (index, element)
        assert!(try_parse_expr("for i, x in arr { }").is_ok());
    }

    #[test]
    fn test_parse_range_expressions() {
        // Basic range expressions
        assert!(try_parse_expr("0..10").is_ok());
        assert!(try_parse_expr("0..=10").is_ok());
        assert!(try_parse_expr("a..b").is_ok());

        // Range has lowest precedence: 1+2..3+4 is (1+2)..(3+4)
        assert!(try_parse_expr("1 + 2 .. 3 + 4").is_ok());
        assert!(try_parse_expr("n - 1 .. n + 1").is_ok());

        // Range as first-class expression
        assert!(try_parse_expr("(0..10)[5]").is_ok());
        assert!(try_parse_expr("len(0..10)").is_ok());

        // Range with function calls
        assert!(try_parse_expr("0..len(arr)").is_ok());
        assert!(try_parse_expr("start()..end()").is_ok());

        // Range in various contexts
        let input = r#"
            fn test(bundle) {
                let indices = 0..10;
                let r = if cond { 0..5 } else { 10..15 };
                for i in 0..len(arr) { }
            }
        "#;
        assert!(try_parse(input).is_ok());
    }

    #[test]
    fn test_parse_match() {
        let input = r#"match x {
            1 => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Array patterns
        let input = r#"match x {
            [a, b] => { },
            [first, second, third] => { },
            [] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Mixed patterns
        let input = r#"match x {
            [a, b] => { },
            {key: value} => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Rest patterns with ..
        let input = r#"match x {
            [first, ..rest] => { },
            [..rest, last] => { },
            [first, ..middle, last] => { },
            [..all] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Rest patterns with whitespace (permissive)
        let input = r#"match x {
            [first, .. rest] => { },
            [first,  ..  middle  , last] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());
    }

    #[test]
    fn test_parse_program() {
        let input = r#"
            import std.bpsec;
            import "../common/utils.flt" as utils;

            const MAX_HOPS = 16;

            fn check_hops(bundle) {
                if bundle.hops > MAX_HOPS {
                    drop();
                }
            }

            fn helper(x) {
                return x + 1;
            }
        "#;
        let result = try_parse(input);
        assert!(result.is_ok(), "Parse error");
        let program = result.unwrap();
        assert_eq!(program.imports.len(), 2);
        assert_eq!(program.constants.len(), 1);
        assert_eq!(program.functions.len(), 2);
    }

    #[test]
    fn test_parse_with() {
        // Single variable - simple statement form
        let input = r#"
            fn test(bundle) {
                with x = bundle.field;
                if is_some(x) {
                    x += 1;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Array destructuring
        let input = r#"
            fn test(bundle) {
                with [a, b] = arr;
                if is_some(a) && is_some(b) {
                    a = 1;
                    b = 2;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Rest patterns
        let input = r#"
            fn test(bundle) {
                with [first, ..rest] = arr;
                if is_some(first) {
                    first = 0;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Nested patterns
        let input = r#"
            fn test(bundle) {
                with [a, [b, c]] = nested;
                if is_some(b) && is_some(c) {
                    b = 10;
                    c = 20;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Type narrowing with type patterns (replaces as_X)
        let input = r#"
            fn test(bundle) {
                with x = arr[0];
                with UInt(n) = x;
                if is_some(n) {
                    n += 1;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());
    }

    #[test]
    fn test_parse_let_patterns() {
        // Simple variable (backwards compatible)
        let input = r#"
            fn test(bundle) {
                let x = 42;
                let name = "hello";
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Array destructuring
        let input = r#"
            fn test(bundle) {
                let [a, b] = arr;
                let [first, second, third] = triple;
                let [x] = single;
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Rest patterns in let
        let input = r#"
            fn test(bundle) {
                let [first, ..rest] = arr;
                let [head, ..middle, last] = arr;
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Nested patterns
        let input = r#"
            fn test(bundle) {
                let [a, [b, c]] = nested;
            }
        "#;
        assert!(try_parse(input).is_ok());
    }

    #[test]
    fn test_parse_if_let_patterns() {
        // if let with single variable
        assert!(try_parse_expr("if let x = expr { }").is_ok());
        assert!(try_parse_expr("if let x = to_uint(y) { }").is_ok());

        // if let with array pattern
        assert!(try_parse_expr("if let [a, b] = arr { }").is_ok());
        assert!(try_parse_expr("if let [first, second, third] = arr { }").is_ok());

        // if let with rest pattern
        assert!(try_parse_expr("if let [first, ..rest] = arr { }").is_ok());

        // if let with nested patterns
        assert!(try_parse_expr("if let [a, [b, c]] = nested { }").is_ok());

        // if let chained with &&
        assert!(try_parse_expr("if let [a, b] = arr && a > 0 { }").is_ok());
        assert!(try_parse_expr("if cond && let [a, b] = arr { }").is_ok());
        assert!(try_parse_expr("if let x = a && let y = b && x < y { }").is_ok());

        // if let with else
        assert!(try_parse_expr("if let [a, b] = arr { } else { }").is_ok());
    }

    #[test]
    fn test_parse_if_with() {
        // if with - by-reference bindings in conditional
        assert!(try_parse_expr("if with x = arr[0] { }").is_ok());
        assert!(try_parse_expr("if with [a, b] = arr { }").is_ok());

        // if with with rest pattern
        assert!(try_parse_expr("if with [first, ..rest] = arr { }").is_ok());

        // if with chained with &&
        assert!(try_parse_expr("if with x = arr[0] && x > 0 { }").is_ok());
        assert!(try_parse_expr("if cond && with x = arr[0] { }").is_ok());
        assert!(try_parse_expr("if with x = a && with y = b { }").is_ok());

        // Mixed if let and if with
        assert!(try_parse_expr("if let x = to_uint(a) && with y = arr[0] { }").is_ok());

        // if with in program context
        let input = r#"
            fn test(bundle) {
                if with x = arr[0] {
                    x += 1;
                }
                if with [a, b] = pair && a > 0 {
                    b = a + 1;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());
    }

    #[test]
    fn test_parse_ignore_rest_pattern() {
        // .. without identifier (ignore rest)
        assert!(try_parse_expr("match x { [a, ..] => { }, _ => { } }").is_ok());
        assert!(try_parse_expr("match x { [.., last] => { }, _ => { } }").is_ok());
        assert!(try_parse_expr("match x { [first, .., last] => { }, _ => { } }").is_ok());

        // in let statements
        let input = r#"
            fn test(bundle) {
                let [first, ..] = arr;
                let [head, .., tail] = arr;
            }
        "#;
        assert!(try_parse(input).is_ok());

        // in with statements
        let input = r#"
            fn test(bundle) {
                with [first, ..] = arr;
            }
        "#;
        assert!(try_parse(input).is_ok());

        // in if let / if with
        assert!(try_parse_expr("if let [a, b, ..] = arr { }").is_ok());
        assert!(try_parse_expr("if with [first, ..] = arr { }").is_ok());

        // Mixed with captured rest
        let input = r#"match x {
            [a, ..] => { },
            [first, ..rest] => { },
            [head, .., tail] => { },
            [a, ..middle, z] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());
    }

    #[test]
    fn test_parse_let_in_match_arms() {
        // Match without let (by-reference, default)
        let input = r#"match x {
            [a, b] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Match with let prefix (by-value)
        let input = r#"match x {
            let [a, b] => { },
            let y => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Mixed let and non-let arms
        let input = r#"match x {
            [a, b] => { },
            let [c, d] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Let with guards
        let input = r#"match x {
            let [a, b] if a > 0 => { },
            let y if y < 10 => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Let with rest patterns
        let input = r#"match x {
            let [first, ..rest] => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());
    }

    #[test]
    fn test_parse_type_patterns() {
        // Type pattern without binding (just matches type)
        let input = r#"match x {
            UInt => { },
            Text => { },
            Array => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Type pattern with simple binding
        let input = r#"match x {
            UInt(n) => { },
            Text(s) => { },
            Bool(b) => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Type pattern with nested destructuring
        let input = r#"match x {
            Array([a, b]) => { },
            Array([first, ..rest]) => { },
            Map({key: value}) => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // Type pattern in let statement
        let input = r#"
            fn test(bundle) {
                let UInt(n) = arr[0];
                let Text(s) = bundle.name;
                let Array([first, second]) = data;
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Type pattern in with statement
        let input = r#"
            fn test(bundle) {
                with UInt(n) = arr[0];
                if is_some(n) {
                    n += 1;
                }
            }
        "#;
        assert!(try_parse(input).is_ok());

        // Type pattern in if let
        assert!(try_parse_expr("if let UInt(n) = value { }").is_ok());
        assert!(try_parse_expr("if let Text(s) = value && len(s) > 0 { }").is_ok());

        // Type pattern in if with
        assert!(try_parse_expr("if with UInt(n) = value { n += 1; }").is_ok());

        // Type pattern with wildcard
        let input = r#"match x {
            UInt(_) => { },
            Text(_) => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());

        // All type names
        let input = r#"match x {
            UInt(a) => { },
            Int(b) => { },
            Float(c) => { },
            Bool(d) => { },
            Text(e) => { },
            Bytes(f) => { },
            Array(g) => { },
            Map(h) => { },
            _ => { },
        }"#;
        assert!(try_parse_expr(input).is_ok());
    }

    #[test]
    fn test_parse_diagnostics() {
        // Test that parse errors are converted to diagnostics
        let mut diags = Diagnostics::new();

        // Valid input should succeed
        let result = parse("fn test() { }", &mut diags);
        assert!(result.is_some());
        assert!(!diags.has_errors());

        // Invalid input should fail and emit diagnostics
        let mut diags = Diagnostics::new();
        let result = parse("fn { }", &mut diags);
        assert!(result.is_none());
        assert!(diags.has_errors());
        assert!(diags.error_count() >= 1);

        // Check the diagnostic has appropriate code and span
        let errors: Vec<_> = diags.errors().collect();
        assert!(!errors.is_empty());
        assert!(errors[0].span.is_some());
    }

    #[test]
    fn test_parse_expr_diagnostics() {
        // Test expression parsing with diagnostics
        let mut diags = Diagnostics::new();

        // Valid expression
        let result = parse_expression("1 + 2", &mut diags);
        assert!(result.is_some());
        assert!(!diags.has_errors());

        // Invalid expression
        let mut diags = Diagnostics::new();
        let result = parse_expression("1 +", &mut diags);
        assert!(result.is_none());
        assert!(diags.has_errors());
    }
}
