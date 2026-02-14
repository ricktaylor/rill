use chumsky::span::SimpleSpan;

// ============================================================================
// Span Types
// ============================================================================

/// Source span - byte offsets into the source text
pub type Span = SimpleSpan<usize>;

/// A value with its source location
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            node: f(self.node),
            span: self.span,
        }
    }

    pub fn as_ref(&self) -> Spanned<&T> {
        Spanned {
            node: &self.node,
            span: self.span,
        }
    }
}

impl<T: PartialEq> PartialEq for Spanned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node // Ignore span for equality
    }
}

impl<T: Eq> Eq for Spanned<T> {}

impl<T: std::hash::Hash> std::hash::Hash for Spanned<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.node.hash(state); // Ignore span for hashing
    }
}

// ============================================================================
// Identifiers
// ============================================================================

// Identifier for variables, functions, etc.
// Must follow identifier rules (no spaces, start with letter/underscore, etc.)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identifier(pub String);

// ============================================================================
// Spanned Type Aliases
// ============================================================================

/// Expression with source span
pub type Expr = Spanned<Expression>;

/// Statement with source span
pub type Stmt = Spanned<Statement>;

/// Pattern with source span
pub type Pat = Spanned<Pattern>;

// ============================================================================
// Program Structure
// ============================================================================

pub struct Program {
    pub imports: Vec<Spanned<Import>>,
    pub constants: Vec<Spanned<Constant>>,
    pub functions: Vec<Spanned<Function>>,
}

#[derive(Debug, Clone)]
pub struct Constant {
    pub pattern: Pat,      // Pattern to bind (match failure = compile error)
    pub value: Expression, // Compiler verifies const-evaluability
}

#[derive(Debug, Clone)]
pub struct Import {
    pub path: ImportPath,
    pub alias: Option<Identifier>, // None = use default name, Some = explicit alias
}

#[derive(Debug, Clone)]
pub enum ImportPath {
    // Standard library: std.bpsec, std.cbor.utils
    Stdlib(Vec<Identifier>),

    // File path: "../common/bundle_age.flt"
    File(String),
}

// ============================================================================
// Attributes
// ============================================================================

/// Function attribute: #[name] or #[name(args)]
#[derive(Debug, Clone)]
pub struct Attribute {
    pub name: Identifier,
    pub args: Vec<AttributeArg>,
}

/// Argument to an attribute
#[derive(Debug, Clone)]
pub enum AttributeArg {
    /// Flag identifier: `export`, `required`
    Flag(Identifier),

    /// Positional literal value: `5000`, `"text"`
    Literal(Literal),

    /// Named value: `timeout: 5000`, `reason: "deprecated"`
    Named { key: Identifier, value: Literal },
}

// ============================================================================
// Functions
// ============================================================================

/// Function parameter with binding mode
/// Default is by-reference; `let` prefix makes it by-value
#[derive(Debug, Clone)]
pub struct FunctionParam {
    /// Parameter name
    pub name: Identifier,
    /// true if `let` prefix (by-value copy), false for by-reference (default)
    pub is_value: bool,
}

/// Function definition with optional attributes
/// All functions use the same structure; the driver selects entry points
/// based on signatures and attributes, not syntax.
#[derive(Debug, Clone)]
pub struct Function {
    /// Attributes: #[export], #[after(validate)], etc.
    pub attributes: Vec<Spanned<Attribute>>,
    /// Function name
    pub name: Identifier,
    /// Parameters with binding mode
    pub params: Vec<FunctionParam>,
    /// Rest parameter: `..args` captures excess arguments as Array
    /// Uses same binding mode semantics (by-ref default, `let` for by-value)
    pub rest_param: Option<FunctionParam>,
    /// Function body
    pub statements: Vec<Stmt>,
    /// Final expression (if block ends without semicolon)
    pub final_expr: Option<Box<Expression>>,
}

#[derive(Debug, Clone)]
pub enum Statement {
    // Variable declaration with pattern: let x = 5; or let [a, b] = expr;
    // Creates copies of values (value semantics, always by-value)
    // Pattern can be:
    //   - Single variable: let x = expr;
    //   - Array destructure: let [a, b, c] = arr;
    //   - With rest: let [first, ..rest] = arr;
    VarDecl {
        pattern: Pat,
        initializer: Expression,
    },

    // Reference binding with pattern: with x = expr; or with [a, b] = arr;
    // Creates references to matched locations (reference semantics)
    // Mutations through pattern bindings affect the original locations
    // Pattern matching is total: if pattern doesn't match, all variables are undefined
    // Use is_some() to check presence
    // Patterns supported:
    //   - Single variable: with x = expr;
    //   - Array destructure: with [a, b] = arr;
    //   - Rest patterns: with [first, ..rest] = arr;
    // All bindings are by-reference (use `let` for by-value copies)
    With {
        pattern: Pat,
        value: Expression,
    },

    // Return statement: return; or return value;
    // Returns a value to the caller
    Return {
        value: Option<Expression>,
    },

    // Expression as statement (function calls, etc.)
    Expression(Expression),

    // Loop control (can break with a value to return from loop expression)
    Break {
        value: Option<Expression>,
    },
    Continue,
}

#[derive(Debug, Clone)]
pub enum Expression {
    Literal(Literal),
    Variable(Identifier),
    /// Qualified name for namespaced function calls: namespace::name
    /// Only valid as target of a function call (e.g., bpsec::validate())
    QualifiedName {
        namespace: Identifier,
        name: Identifier,
    },
    BinaryOp {
        left: Box<Expression>,
        op: BinaryOperator,
        right: Box<Expression>,
    },

    // Assignment expression: target = value or target op= value
    // Returns the assigned value (or undefined if lvalue is invalid)
    // Right-associative: a = b = c parses as a = (b = c)
    // Valid lvalues: Variable, ArrayAccess, MemberAccess, BinaryOp(BitTest)
    Assignment {
        target: Box<Expression>,
        op: AssignmentOp,
        value: Box<Expression>,
    },

    UnaryOp {
        op: UnaryOperator,
        operand: Box<Expression>,
    },
    FunctionCall {
        namespace: Option<Identifier>, // e.g., "bpsec" in bpsec::validate()
        name: Identifier,
        arguments: Vec<Expression>,
    },
    // Array/Map access: arr[i]
    // Returns missing if array/index is missing or out of bounds
    ArrayAccess {
        array: Box<Expression>,
        index: Box<Expression>,
    },

    // Member access (CBOR map key): obj.foo => obj[Text("foo")]
    // Returns missing if object or key is missing
    MemberAccess {
        object: Box<Expression>,
        member: Box<Expression>, // CBOR map key (any CBOR type)
    },

    // Block expression: { statements; final_expr }
    Block {
        statements: Vec<Stmt>,
        final_expr: Option<Box<Expression>>, // Last expr without semicolon
    },

    // If expression with optional let bindings
    // Allows chaining boolean expressions and let bindings with &&
    // Examples:
    //   if condition { }
    //   if let x = expr { }
    //   if condition && let x = expr { }
    //   if let x = expr && x > 0 { }
    // All conditions are AND'ed - variables bound if ALL conditions succeed
    // Variables are in scope for later conditions AND the then-block
    // Use `with` for reference bindings, `if let` for value bindings
    // NOTE: No ? needed - the implicit presence check IS the point of if let/if with
    If {
        conditions: Vec<IfCondition>, // All must be true (short-circuit AND)
        then_block: Vec<Stmt>,
        then_expr: Option<Box<Expression>>, // Final expr without semicolon
        else_block: Option<Vec<Stmt>>,
        else_expr: Option<Box<Expression>>, // Final expr without semicolon
    },

    // While loop expression: while condition { }
    While {
        condition: Box<Expression>,
        body: Vec<Stmt>,
        body_expr: Option<Box<Expression>>, // Final expr without semicolon
    },

    // Infinite loop expression: loop { }
    Loop {
        body: Vec<Stmt>,
        body_expr: Option<Box<Expression>>, // Final expr without semicolon
    },

    // Iterator-based for loop expression
    // Reference binding (default): for x in arr { } - x refers to each element
    // Value binding (explicit): for let x in arr { } - x is a copy of each element
    // Destructuring patterns for maps: for [k, v] in map { }
    // For destructuring: key is always by-value/immutable, `let` controls value binding
    For {
        binding_is_value: bool, // true if `let` keyword present (controls value binding)
        binding: ForBinding,    // Single variable or destructuring pattern
        iterable: Box<Expression>, // Array, Map, or Range expression
        body: Vec<Stmt>,
        body_expr: Option<Box<Expression>>, // Final expr without semicolon
    },

    // Pattern matching: match value { pattern => body, ... }
    Match {
        value: Box<Expression>,
        arms: Vec<MatchArm>,
    },

    // Range expression: 0..10 or 0..=10
    // Produces an Array, evaluated lazily during iteration
    // Can be used anywhere an expression is expected
    Range {
        start: Box<Expression>,
        end: Box<Expression>,
        inclusive: bool, // true for ..=, false for ..
    },
}

// Conditions in if expressions (all AND'ed together with &&)
#[derive(Debug, Clone)]
pub enum IfCondition {
    // Boolean expression: if x > 5, if is_some(x)
    Bool(Expression),

    // Let binding: if let pattern = expr
    // Pattern matching with by-value binding (copies)
    // Body runs only if pattern matches (all variables present)
    // Variables in scope for later conditions and the then-block
    Let { pattern: Pat, value: Expression },

    // With binding: if with pattern = expr
    // Pattern matching with by-reference binding (mutations affect original)
    // Body runs only if pattern matches (all variables present)
    // Variables in scope for later conditions and the then-block
    With { pattern: Pat, value: Expression },
}

// Binding in for loops - either a single variable or destructuring pattern
#[derive(Debug, Clone)]
pub enum ForBinding {
    // Single variable: for x in arr { }
    // Binding mode (ref/value) controlled by presence of `let`
    Variable(Identifier),

    // Array destructuring: for [k, v] in map { }
    // Key (first element) is ALWAYS by-value and immutable (mutation is compile error)
    // Value (second element) binding mode controlled by presence of `let`
    Array(Vec<Identifier>),
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub binding_is_value: bool, // true if `let` prefix (by-value), false for by-reference
    pub pattern: Pat,
    pub guard: Option<Expression>, // Optional if condition
    pub body: Vec<Stmt>,
    pub body_expr: Option<Box<Expression>>, // Final expr without semicolon
}

#[derive(Debug, Clone)]
pub enum Pattern {
    // Wildcard pattern: _
    Wildcard,

    // Literal pattern: 42, "hello", true
    Literal(Literal),

    // Variable binding: x (binds the value to variable x)
    Variable(Identifier),

    // Array patterns: [a, b, c]
    // All bindings are by-reference
    Array(Vec<Pat>),

    // Array with rest: [first, ..rest] or [first, ..] or [first, ..middle, last]
    // If rest is Some(id): captures remaining elements in variable
    // If rest is None: matches but ignores remaining elements (.. without variable)
    // Rest is ALWAYS a valid collection (empty if zero elements, never missing)
    // Pattern fails to match if non-rest parts can't be satisfied
    // Whitespace around .. is permitted: [a, .. rest] is valid
    ArrayRest {
        before: Vec<Pat>,
        rest: Option<Identifier>, // None means ".." without variable (ignore rest)
        after: Vec<Pat>,
    },

    // Map pattern: {key_pattern: value_pattern, ...}
    // Can match on any CBOR key type: {42: x}, {"name": n}, etc.
    Map(Vec<(Pat, Pat)>),

    // Type pattern with optional binding
    // Examples:
    //   UInt           - matches UInt type, no binding
    //   UInt(x)        - matches UInt type, binds x to the value
    //   UInt([a, b])   - matches UInt type (though unlikely), binds nested pattern
    //   Array([a, b])  - matches Array type, destructures into a, b
    // Type names: UInt, Int, Float, Bool, Text, Bytes, Array, Map
    Type {
        type_name: Identifier,
        binding: Option<Box<Pat>>, // None = just match, Some = bind
    },
}

#[derive(Debug, Clone)]
pub enum Literal {
    Bool(bool),
    UInt(u64),
    Int(i64),
    Float(f64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Expression>),             // CBOR array literal
    Map(Vec<(Expression, Expression)>), // CBOR map literal
}

#[derive(Debug, Clone)]
pub enum UnaryOperator {
    Negate,     // -x (arithmetic negation)
    Not,        // !x (logical NOT)
    BitwiseNot, // ~x (bitwise complement)
}

#[derive(Debug, Clone)]
pub enum BinaryOperator {
    // Arithmetic
    Add,      // +
    Subtract, // -
    Multiply, // *
    Divide,   // /
    Modulo,   // %

    // Comparison
    Equal,        // ==
    NotEqual,     // !=
    Less,         // <
    LessEqual,    // <=
    Greater,      // >
    GreaterEqual, // >=

    // Logical
    And, // &&
    Or,  // ||

    // Bitwise
    BitwiseAnd, // &
    BitwiseOr,  // |
    BitwiseXor, // ^
    ShiftLeft,  // <<
    ShiftRight, // >>

    // Bit test
    BitTest, // @ - returns true if bit B is set in X (X @ B)
}

#[derive(Debug, Clone)]
pub enum AssignmentOp {
    Assign,    // =
    AddAssign, // +=
    SubAssign, // -=
    MulAssign, // *=
    DivAssign, // /=
    ModAssign, // %=
    AndAssign, // &=
    OrAssign,  // |=
    XorAssign, // ^=
    ShlAssign, // <<=
    ShrAssign, // >>=
}
