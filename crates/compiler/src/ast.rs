//! Abstract syntax tree.
//!
//! Every node carries a [`Span`] pointing at the bytes in the source file it
//! came from. Names and string literals also keep the original text so a later
//! phase can re-validate without re-lexing.
//!
//! The AST is *concrete enough* to render diagnostics that reference precise
//! source ranges, but it does no semantic work — keywords like `pub`, `async`,
//! or `static` are stored as flags on the relevant item, not as separate
//! nodes.

use crate::span::Span;
use crate::token::IntBase;

// ===========================================================================
// Common helpers
// ===========================================================================

/// An identifier captured from the source.
///
/// `name` is the *exact* text between the identifier's start and end byte
/// offsets — no normalization, no interning. Spans always point at this slice.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl Ident {
    pub fn new(name: impl Into<String>, span: Span) -> Self {
        Self { name: name.into(), span }
    }
}

/// `pub` modifier with the span of the keyword, or absent.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Visibility {
    Public(Span),
    Private,
}

impl Visibility {
    pub fn is_public(&self) -> bool {
        matches!(self, Visibility::Public(_))
    }
}

/// A `///` or `//!` doc comment as captured by the lexer.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct DocComment {
    /// The original text including the leading `///` or `//!`.
    pub text: String,
    pub span: Span,
    pub is_inner: bool,
}

/// `@Decorator` or `@Decorator(arg1, kw = arg2, ...)`.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Attribute {
    pub name: Ident,
    pub args: Vec<AttrArg>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum AttrArg {
    Positional(Expr),
    Named { name: Ident, value: Expr, span: Span },
}

// ===========================================================================
// Top-level module
// ===========================================================================

/// A whole source file as a sequence of items, with optional `//!` inner docs
/// at the top.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Module {
    pub inner_docs: Vec<DocComment>,
    pub items: Vec<Item>,
    pub span: Span,
}

// ===========================================================================
// Items (top-level declarations)
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Item {
    pub docs: Vec<DocComment>,
    pub attrs: Vec<Attribute>,
    pub visibility: Visibility,
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ItemKind {
    Var(VarItem),
    Function(FunctionItem),
    Struct(StructItem),
    Interface(InterfaceItem),
    TypeAlias(TypeAliasItem),
    Module(ModuleItem),
    Extend(ExtendItem),
    /// `extern function`, `extern struct`, `extern type`, `extern var`
    Extern(ExternItem),
    Import(ImportItem),
}

// ---- var --------------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct VarItem {
    pub name: Ident,
    pub ty: Option<Type>,
    pub init: Expr,
}

// ---- function ---------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct FunctionItem {
    pub name: Ident,
    pub generics: Option<GenericParams>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub is_async: bool,
    pub body: Option<Block>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Param {
    pub kind: ParamKind,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ParamKind {
    /// `self` — only valid as first parameter inside an `extend`/`interface`
    /// method.
    SelfParam,
    Normal { name: Ident, ty: Type },
}

// ---- generic params ---------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct GenericParams {
    pub params: Vec<GenericParam>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct GenericParam {
    pub name: Ident,
    /// Trait/interface bounds (`T: A + B`).
    pub bounds: Vec<Type>,
    /// Optional default (`T = Self`).
    pub default: Option<Type>,
    pub span: Span,
}

// ---- struct -----------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct StructItem {
    pub name: Ident,
    pub generics: Option<GenericParams>,
    pub is_extern: bool,
    pub kind: StructKind,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum StructKind {
    /// `struct Red;`
    Unit,
    /// `struct Pair(pub i64, pub i64)`
    Tuple(Vec<TupleField>),
    /// `struct Person { name: str, age: i64 }`
    Record(Vec<StructField>),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct TupleField {
    pub visibility: Visibility,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct StructField {
    pub docs: Vec<DocComment>,
    pub attrs: Vec<Attribute>,
    pub visibility: Visibility,
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

// ---- interface --------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct InterfaceItem {
    pub name: Ident,
    pub generics: Option<GenericParams>,
    pub supers: Vec<Type>,
    pub members: Vec<InterfaceMember>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct InterfaceMember {
    pub docs: Vec<DocComment>,
    pub attrs: Vec<Attribute>,
    /// `static function foo(...)` — also true for any method without a `self`
    /// param so the consumer doesn't have to look at the params.
    pub is_static_keyword: bool,
    pub function: FunctionSig,
    pub default_body: Option<Block>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct FunctionSig {
    pub name: Ident,
    pub generics: Option<GenericParams>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub is_async: bool,
}

// ---- type alias -------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct TypeAliasItem {
    pub name: Ident,
    pub generics: Option<GenericParams>,
    pub aliased: Type,
}

// ---- mod --------------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ModuleItem {
    pub name: Ident,
    pub kind: ModuleKind,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ModuleKind {
    /// `mod foo` — body lives in another file.
    External,
    /// `mod foo { ... }`
    Inline {
        inner_docs: Vec<DocComment>,
        items: Vec<Item>,
    },
}

// ---- extend -----------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ExtendItem {
    pub generics: Option<GenericParams>,
    pub target: Type,
    pub interfaces: Vec<Type>,
    pub members: Vec<ExtendMember>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ExtendMember {
    pub docs: Vec<DocComment>,
    pub attrs: Vec<Attribute>,
    pub visibility: Visibility,
    /// True if declared with `static function …`.
    pub is_static_keyword: bool,
    pub function: FunctionItem,
    pub span: Span,
}

// ---- extern -----------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ExternItem {
    Function(FunctionItem),
    Struct(StructItem),
    /// `extern type Name`
    OpaqueType(Ident),
    /// `extern var name: T`
    Var { name: Ident, ty: Type },
}

// ---- import -----------------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ImportItem {
    pub kind: ImportKind,
    pub path: StringLit,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ImportKind {
    /// `import "path"` — pulls in extension methods only.
    Ambient,
    /// `import "path" as Name`
    Namespace(Ident),
    /// `import { a, b as c } from "path"`
    Named(Vec<ImportName>),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ImportName {
    pub name: Ident,
    pub alias: Option<Ident>,
    pub span: Span,
}

// ===========================================================================
// Types
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum TypeKind {
    /// `Foo` or `Foo<T1, T2>`
    Named { name: Ident, generics: Vec<Type> },
    /// `(T1, T2, …)` — never zero-length.
    Tuple(Vec<Type>),
    /// `(T1, T2) => R`
    Function { params: Vec<Type>, ret: Box<Type> },
    /// `extern (name: T, …) => R`
    ExternFunction { params: Vec<ExternParamType>, ret: Box<Type> },
    /// `T | U | V` — never empty, never single-element (single ⇒ collapse to inner).
    Union(Vec<Type>),
    /// `*T`
    Pointer(Box<Type>),
    /// `[T; N]` — FFI only; `len` is left as a generic expression and is
    /// validated downstream.
    Array { elem: Box<Type>, len: Box<Expr> },
    /// `Self`
    SelfType,
    /// `(T)` — explicit grouping that survives into the AST so diagnostics
    /// can point at user-written parens.
    Paren(Box<Type>),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ExternParamType {
    pub name: Option<Ident>,
    pub ty: Type,
    pub span: Span,
}

// ===========================================================================
// Statements
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum StmtKind {
    /// `var pat [: T] = expr;`
    Var(LocalVar),
    /// `lvalue = expr;`
    Assign { target: Expr, value: Expr },
    /// An expression terminated by `;`.
    Expr(Expr),
    /// Inner item declaration — block-level `function`, `struct`, etc.
    Item(Box<Item>),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct LocalVar {
    pub pattern: Pattern,
    pub ty: Option<Type>,
    pub init: Expr,
}

// ===========================================================================
// Blocks
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// Optional trailing expression — if present, the block evaluates to it.
    pub trailing: Option<Box<Expr>>,
    pub span: Span,
}

// ===========================================================================
// Expressions
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ExprKind {
    // Literals ---------------------------------------------------------------
    Int(IntLit),
    Float(FloatLit),
    Bool(bool),
    Null,
    Char(CharLit),
    Str(StringLit),

    // Names ------------------------------------------------------------------
    /// A bare identifier in expression position.
    Ident(Ident),
    /// `self`
    SelfExpr,
    /// `_` — only legal on the LHS of an assignment as a discard.
    Underscore,

    // Composite --------------------------------------------------------------
    /// `(a, b, c)` — always ≥ 2 elements; one-element is `Paren`.
    Tuple(Vec<Expr>),
    Paren(Box<Expr>),
    /// `[a, b, c]`
    List(Vec<Expr>),
    /// `Foo { x: 1, y: 2, ..base }`
    StructLit {
        path: TypePath,
        fields: Vec<FieldInit>,
        spread: Option<Box<Expr>>,
    },

    // Operators --------------------------------------------------------------
    Unary { op: UnaryOp, op_span: Span, operand: Box<Expr> },
    Binary { op: BinaryOp, op_span: Span, left: Box<Expr>, right: Box<Expr> },
    /// `expr as T` / `expr is T`
    Cast { op: CastOp, op_span: Span, expr: Box<Expr>, ty: Box<Type> },

    // Postfix accesses -------------------------------------------------------
    Field { receiver: Box<Expr>, name: Ident },
    TupleIndex { receiver: Box<Expr>, index: u32, index_span: Span },
    Call {
        callee: Box<Expr>,
        /// Explicit generic args: `id<i64>(42)`.
        generics: Vec<Type>,
        args: Vec<Expr>,
        /// `xs.map { it * 2 }` — closure given as trailing block.
        trailing_closure: Option<Box<Expr>>,
    },
    Index { receiver: Box<Expr>, index: Box<Expr> },
    Try { expr: Box<Expr>, q_span: Span },
    /// `&expr` — address-of (FFI).
    Ref { expr: Box<Expr>, amp_span: Span },
    /// `*expr` — pointer dereference.
    Deref { expr: Box<Expr>, star_span: Span },
    Await { expr: Box<Expr>, kw_span: Span },

    // Control flow ----------------------------------------------------------
    If {
        cond: Box<Expr>,
        then_block: Block,
        else_branch: Option<ElseBranch>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    Block(Block),
    Loop(Block),
    While { cond: Box<Expr>, body: Block },
    For {
        pattern: Pattern,
        in_async: bool,
        iter: Box<Expr>,
        body: Block,
    },
    Return(Option<Box<Expr>>),
    Break(Option<Box<Expr>>),
    Continue,

    // Function-shaped expressions -------------------------------------------
    /// `(x) => body`, `() => body`, `(x: i32): i32 => body`, with optional
    /// `async` between params and `=>`.
    Closure {
        params: Vec<ClosureParam>,
        return_type: Option<Type>,
        is_async: bool,
        body: Box<Expr>,
    },
    /// `function(x: i32): i32 { … }` — anonymous function expression.
    AnonFn(Box<FunctionItem>),
    /// `async { … }` — zero-arg inline future literal.
    AsyncBlock(Block),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct TypePath {
    pub name: Ident,
    pub generics: Vec<Type>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct FieldInit {
    pub name: Ident,
    /// `None` = field-init shorthand `Foo { x }`.
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ElseBranch {
    /// `else if cond { … }` — the boxed expression is always an `If`.
    If(Box<Expr>),
    Block(Block),
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ClosureParam {
    pub name: Ident,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum UnaryOp {
    Neg,
    /// `!` — logical or bitwise depending on operand type.
    Not,
    /// `~` — accepted by the parser for compatibility; resolves to `Not` on
    /// integers downstream.
    BitNot,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum BinaryOp {
    Add, Sub, Mul, Div, Rem,
    Eq, Ne, Lt, Le, Gt, Ge,
    And, Or,
    BitAnd, BitOr, BitXor,
    Shl, Shr,
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum CastOp {
    As,
    Is,
}

// ---- literal payloads -------------------------------------------------------

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct IntLit {
    /// Raw digit text without the base prefix and without underscores stripped.
    pub raw: String,
    pub base: IntBase,
    pub suffix: Option<String>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct FloatLit {
    pub raw: String,
    pub suffix: Option<String>,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct CharLit {
    /// Exact source text including quotes, e.g. `'\n'`.
    pub raw: String,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct StringLit {
    pub parts: Vec<StringPart>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum StringPart {
    /// A run of literal text. `text` is the raw slice (escapes not processed).
    Text { text: String, span: Span },
    /// `$identifier` — the bare identifier.
    Ident(Ident),
    /// `${expr}`
    Expr(Expr),
}

// ===========================================================================
// Match arms
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

// ===========================================================================
// Patterns
// ===========================================================================

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum PatternKind {
    /// `_`
    Wildcard,
    /// `name`
    Binding(Ident),
    /// `42`, `"x"`, `'c'`, `true`, `false`, `null`, `-1`.
    Literal(Box<Expr>),
    /// `T x` or just `T` — binds a value if `T` matches as a union variant.
    /// Used for primitive-type and union-variant matching (`i64 n`, `i64`).
    TypeBinding { ty: Type, binding: Option<Ident> },
    /// `Red`
    UnitPath(TypePath),
    /// `Some(a, b)` — positional destructuring.
    TupleStruct { path: TypePath, fields: Vec<Pattern>, rest: Option<RestPattern> },
    /// `Person { name, age }`, `Person { name, .. }`
    RecordStruct { path: TypePath, fields: Vec<FieldPattern>, has_rest: bool },
    /// `(a, b)`, `(a, ..rest, b)`
    Tuple { elems: Vec<Pattern>, rest: Option<(usize, RestPattern)> },
    /// `[a, b, c]`, `[head, ..tail]`
    List { elems: Vec<Pattern>, rest: Option<(usize, RestPattern)> },
    /// `P1 | P2 | P3`
    Or(Vec<Pattern>),
}

/// Optional `..name` / `..` binding in a rest position.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct RestPattern {
    pub name: Option<Ident>,
    pub span: Span,
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub struct FieldPattern {
    pub name: Ident,
    /// `None` ⇒ shorthand `Person { name }`.
    pub pattern: Option<Pattern>,
    pub span: Span,
}
