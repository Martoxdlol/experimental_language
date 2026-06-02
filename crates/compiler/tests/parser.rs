//! Integration tests for the parser. Each test feeds a small snippet through
//! the lexer + parser pipeline and asserts on the resulting AST.

use compiler::ast::*;
use compiler::lexer::lex;
use compiler::parse_diag::{ParseError, ParseErrorKind};
use compiler::parser::parse;
use compiler::span::SourceMap;
use compiler::token::{IntBase, Token};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_src(src: &str) -> (Module, Vec<ParseError>, Vec<Token>) {
    let mut sm = SourceMap::new();
    let file = sm.add_file("t", src);
    let (tokens, lex_errs) = lex(sm.file(file).src.as_str(), file);
    assert!(lex_errs.is_empty(), "lexer errors: {lex_errs:?}");
    let (module, errs) = parse(sm.file(file).src.as_str(), &tokens);
    (module, errs, tokens)
}

fn parse_ok(src: &str) -> Module {
    let (m, e, _) = parse_src(src);
    assert!(
        e.is_empty(),
        "expected no parse errors, got:\n{e:#?}\nsrc:\n{src}"
    );
    m
}

fn parse_expr_ok(src: &str) -> Expr {
    let wrapper = format!("var __x = {src};");
    let m = parse_ok(&wrapper);
    let item = m.items.first().expect("no item");
    match &item.kind {
        ItemKind::Var(v) => v.init.clone(),
        _ => panic!("expected var item"),
    }
}

fn slice<'a>(src: &'a str, span: compiler::span::Span) -> &'a str {
    &src[span.range()]
}

// ---------------------------------------------------------------------------
// Module / basic items
// ---------------------------------------------------------------------------

#[test]
fn empty_source_parses_to_empty_module() {
    let (m, e, _) = parse_src("");
    assert!(e.is_empty());
    assert!(m.items.is_empty());
    assert!(m.inner_docs.is_empty());
}

#[test]
fn whitespace_only_source_parses() {
    let (m, e, _) = parse_src("   \n  \n");
    assert!(e.is_empty());
    assert!(m.items.is_empty());
}

#[test]
fn module_inner_docs_are_collected() {
    let (m, _, _) = parse_src("//! crate-level doc\n//! second line\nvar x = 1;");
    assert_eq!(m.inner_docs.len(), 2);
    assert_eq!(m.items.len(), 1);
}

#[test]
fn var_item_basic() {
    let m = parse_ok("var x = 42;");
    let item = &m.items[0];
    assert_eq!(item.visibility, Visibility::Private);
    match &item.kind {
        ItemKind::Var(v) => {
            assert_eq!(v.name.name, "x");
            assert!(v.ty.is_none());
            assert!(matches!(v.init.kind, ExprKind::Int(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn var_item_with_type() {
    let m = parse_ok("pub var counter: i64 = 0;");
    let item = &m.items[0];
    assert!(item.visibility.is_public());
    match &item.kind {
        ItemKind::Var(v) => {
            assert_eq!(v.name.name, "counter");
            assert!(v.ty.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn doc_and_attribute_attach_to_item() {
    let src = "\
/// First line.
/// Second line.
@Derive(Eq, Hash)
@Repr(C)
pub function foo() {}";
    let m = parse_ok(src);
    let it = &m.items[0];
    assert_eq!(it.docs.len(), 2);
    assert_eq!(it.attrs.len(), 2);
    assert_eq!(it.attrs[0].name.name, "Derive");
    assert_eq!(it.attrs[0].args.len(), 2);
    assert_eq!(it.attrs[1].name.name, "Repr");
}

#[test]
fn attribute_named_arg() {
    let src = r#"@Link(lib = "c", kind = "static") extern function exit(code: i32);"#;
    let m = parse_ok(src);
    let it = &m.items[0];
    assert_eq!(it.attrs.len(), 1);
    match &it.attrs[0].args[0] {
        AttrArg::Named { name, .. } => assert_eq!(name.name, "lib"),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

#[test]
fn function_no_args_no_return() {
    let m = parse_ok("function f() {}");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            assert_eq!(f.name.name, "f");
            assert!(f.params.is_empty());
            assert!(f.return_type.is_none());
            assert!(f.body.is_some());
            assert!(!f.is_async);
        }
        _ => panic!(),
    }
}

#[test]
fn function_with_args_and_return() {
    let m = parse_ok("pub function add(a: i64, b: i64): i64 { a + b }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            assert_eq!(f.params.len(), 2);
            assert!(f.return_type.is_some());
            let body = f.body.as_ref().unwrap();
            assert!(body.trailing.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn function_with_generics_and_bounds() {
    let m = parse_ok("function bigger<T: Ord>(a: T, b: T): T { a }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let g = f.generics.as_ref().unwrap();
            assert_eq!(g.params.len(), 1);
            assert_eq!(g.params[0].name.name, "T");
            assert_eq!(g.params[0].bounds.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn function_multiple_generic_bounds() {
    let m = parse_ok("function f<T: Eq + Hash + Clone>() {}");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let g = f.generics.as_ref().unwrap();
            assert_eq!(g.params[0].bounds.len(), 3);
        }
        _ => panic!(),
    }
}

#[test]
fn function_generic_default() {
    let m = parse_ok("function id<T = i64>(x: T): T { x }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            assert!(f.generics.as_ref().unwrap().params[0].default.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn async_function() {
    let m = parse_ok("function fetch(url: str): str async { url }");
    match &m.items[0].kind {
        ItemKind::Function(f) => assert!(f.is_async),
        _ => panic!(),
    }
}

#[test]
fn self_param_in_extend_method() {
    let m = parse_ok("extend Person { function greet(self): str { self.name } }");
    match &m.items[0].kind {
        ItemKind::Extend(e) => {
            assert_eq!(e.members.len(), 1);
            assert!(matches!(
                e.members[0].function.params[0].kind,
                ParamKind::SelfParam
            ));
        }
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

#[test]
fn record_struct() {
    let m = parse_ok("struct Person { pub name: str, age: i64, }");
    match &m.items[0].kind {
        ItemKind::Struct(s) => match &s.kind {
            StructKind::Record(fs) => {
                assert_eq!(fs.len(), 2);
                assert!(fs[0].visibility.is_public());
                assert!(!fs[1].visibility.is_public());
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn tuple_struct() {
    let m = parse_ok("pub struct Pair(pub i64, pub i64);");
    match &m.items[0].kind {
        ItemKind::Struct(s) => match &s.kind {
            StructKind::Tuple(fs) => {
                assert_eq!(fs.len(), 2);
                assert!(fs[0].visibility.is_public());
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn unit_struct() {
    let m = parse_ok("pub struct Red;");
    match &m.items[0].kind {
        ItemKind::Struct(s) => assert!(matches!(s.kind, StructKind::Unit)),
        _ => panic!(),
    }
}

#[test]
fn generic_struct() {
    let m = parse_ok("struct Wrapper<T> { value: T }");
    match &m.items[0].kind {
        ItemKind::Struct(s) => {
            assert!(s.generics.is_some());
            assert!(matches!(s.kind, StructKind::Record(_)));
        }
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Interfaces & extend
// ---------------------------------------------------------------------------

#[test]
fn interface_with_methods() {
    let src = "\
interface Named {
    function name(self): str;
    function greet(self): str { self.name() }
}";
    let m = parse_ok(src);
    match &m.items[0].kind {
        ItemKind::Interface(i) => {
            assert_eq!(i.members.len(), 2);
            assert!(i.members[0].default_body.is_none());
            assert!(i.members[1].default_body.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn interface_super_traits() {
    let src = "interface Render: Printable + Sized { function draw(self); }";
    let m = parse_ok(src);
    match &m.items[0].kind {
        ItemKind::Interface(i) => assert_eq!(i.supers.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn extend_with_interface_impl() {
    let m = parse_ok("extend<T: Clone> Wrapper<T>: Clone { function clone(self): Self { self } }");
    match &m.items[0].kind {
        ItemKind::Extend(e) => {
            assert!(e.generics.is_some());
            assert_eq!(e.interfaces.len(), 1);
            assert_eq!(e.members.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn extend_static_function_is_just_a_function_without_self() {
    // There is no `static` keyword: a method with no `self` parameter is static.
    let m = parse_ok("extend Wrapper { function new(): Wrapper { Wrapper {} } }");
    match &m.items[0].kind {
        ItemKind::Extend(e) => {
            let params = &e.members[0].function.params;
            assert!(
                params
                    .iter()
                    .all(|p| !matches!(p.kind, ParamKind::SelfParam))
            );
        }
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

#[test]
fn type_alias_simple() {
    let m = parse_ok("type Point = (i64, i64);");
    match &m.items[0].kind {
        ItemKind::TypeAlias(t) => {
            assert_eq!(t.name.name, "Point");
            assert!(matches!(t.aliased.kind, TypeKind::Tuple(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn type_alias_with_union() {
    let m = parse_ok("type Maybe<T> = T | null;");
    match &m.items[0].kind {
        ItemKind::TypeAlias(t) => match &t.aliased.kind {
            TypeKind::Union(alts) => assert_eq!(alts.len(), 2),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn type_alias_function_type() {
    let m = parse_ok("type IntFn = (i64) => i64;");
    match &m.items[0].kind {
        ItemKind::TypeAlias(t) => assert!(matches!(t.aliased.kind, TypeKind::Function { .. })),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

#[test]
fn external_module() {
    let m = parse_ok("pub mod util;");
    match &m.items[0].kind {
        ItemKind::Module(m2) => {
            assert_eq!(m2.name.name, "util");
            assert!(matches!(m2.kind, ModuleKind::External));
        }
        _ => panic!(),
    }
}

#[test]
fn inline_module() {
    let m = parse_ok("mod inner { function f() {} pub var c = 1; }");
    match &m.items[0].kind {
        ItemKind::Module(m2) => match &m2.kind {
            ModuleKind::Inline { items, .. } => assert_eq!(items.len(), 2),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn nested_external_mod_inside_inline_errors() {
    let src = "mod outer { mod inner; }";
    let (_, errs, _) = parse_src(src);
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ParseErrorKind::NestedExternalMod)),
        "expected NestedExternalMod, got: {errs:?}"
    );
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

#[test]
fn ambient_import() {
    let m = parse_ok(r#"import "core:prelude";"#);
    match &m.items[0].kind {
        ItemKind::Import(i) => assert!(matches!(i.kind, ImportKind::Ambient)),
        _ => panic!(),
    }
}

#[test]
fn namespace_import() {
    let m = parse_ok(r#"import "util/log" as Log;"#);
    match &m.items[0].kind {
        ItemKind::Import(i) => match &i.kind {
            ImportKind::Namespace(n) => assert_eq!(n.name, "Log"),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn named_import() {
    let m = parse_ok(r#"import { a, b as c } from "lib";"#);
    match &m.items[0].kind {
        ItemKind::Import(i) => match &i.kind {
            ImportKind::Named(ns) => {
                assert_eq!(ns.len(), 2);
                assert!(ns[0].alias.is_none());
                assert_eq!(ns[1].alias.as_ref().unwrap().name, "c");
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn pub_import() {
    let (m, e, _) = parse_src(r#"pub import "lib";"#);
    assert!(e.is_empty());
    assert!(m.items[0].visibility.is_public());
}

// ---------------------------------------------------------------------------
// Extern declarations
// ---------------------------------------------------------------------------

#[test]
fn extern_function_decl() {
    let m = parse_ok("extern function malloc(n: u64): *u8;");
    match &m.items[0].kind {
        ItemKind::Extern(ExternItem::Function(f)) => {
            assert_eq!(f.name.name, "malloc");
            assert!(f.body.is_none());
        }
        _ => panic!(),
    }
}

#[test]
fn extern_struct() {
    let m = parse_ok("extern struct Buf { pub data: *u8, pub size: u64 }");
    match &m.items[0].kind {
        ItemKind::Extern(ExternItem::Struct(s)) => assert!(s.is_extern),
        _ => panic!(),
    }
}

#[test]
fn extern_type() {
    let m = parse_ok("extern type Sqlite3;");
    match &m.items[0].kind {
        ItemKind::Extern(ExternItem::OpaqueType(name)) => assert_eq!(name.name, "Sqlite3"),
        _ => panic!(),
    }
}

#[test]
fn extern_var() {
    let m = parse_ok("extern var errno: i32;");
    match &m.items[0].kind {
        ItemKind::Extern(ExternItem::Var { name, .. }) => assert_eq!(name.name, "errno"),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Type expressions
// ---------------------------------------------------------------------------

fn parse_type_via_alias(src: &str) -> Type {
    let wrapper = format!("type T = {src};");
    let m = parse_ok(&wrapper);
    match &m.items[0].kind {
        ItemKind::TypeAlias(t) => t.aliased.clone(),
        _ => panic!(),
    }
}

#[test]
fn type_primitive() {
    let t = parse_type_via_alias("i64");
    match t.kind {
        TypeKind::Named { name, generics } => {
            assert_eq!(name.name, "i64");
            assert!(generics.is_empty());
        }
        _ => panic!(),
    }
}

#[test]
fn type_generic_simple() {
    let t = parse_type_via_alias("List<i64>");
    match t.kind {
        TypeKind::Named { generics, .. } => assert_eq!(generics.len(), 1),
        _ => panic!(),
    }
}

#[test]
fn type_nested_generic_with_double_close() {
    let t = parse_type_via_alias("Map<str, List<i64>>");
    match t.kind {
        TypeKind::Named { name, generics } => {
            assert_eq!(name.name, "Map");
            assert_eq!(generics.len(), 2);
        }
        _ => panic!(),
    }
}

#[test]
fn type_pointer() {
    let t = parse_type_via_alias("*u8");
    assert!(matches!(t.kind, TypeKind::Pointer(_)));
}

#[test]
fn type_pointer_to_pointer() {
    let t = parse_type_via_alias("**u8");
    match t.kind {
        TypeKind::Pointer(inner) => assert!(matches!(inner.kind, TypeKind::Pointer(_))),
        _ => panic!(),
    }
}

#[test]
fn type_tuple() {
    let t = parse_type_via_alias("(i64, str, bool)");
    match t.kind {
        TypeKind::Tuple(ts) => assert_eq!(ts.len(), 3),
        _ => panic!(),
    }
}

#[test]
fn type_parenthesized_single() {
    let t = parse_type_via_alias("(i64)");
    assert!(matches!(t.kind, TypeKind::Paren(_)));
}

#[test]
fn type_function() {
    let t = parse_type_via_alias("(i64, i64) => i64");
    match t.kind {
        TypeKind::Function { params, .. } => assert_eq!(params.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn type_function_zero_args() {
    let t = parse_type_via_alias("() => null");
    match t.kind {
        TypeKind::Function { params, .. } => assert!(params.is_empty()),
        _ => panic!(),
    }
}

#[test]
fn type_extern_function() {
    let t = parse_type_via_alias("extern (data: *u8, size: u64) => i32");
    match t.kind {
        TypeKind::ExternFunction { params, .. } => {
            assert_eq!(params.len(), 2);
            assert_eq!(params[0].name.as_ref().unwrap().name, "data");
        }
        _ => panic!(),
    }
}

#[test]
fn type_union_three_variants() {
    let t = parse_type_via_alias("i64 | str | null");
    match t.kind {
        TypeKind::Union(alts) => assert_eq!(alts.len(), 3),
        _ => panic!(),
    }
}

#[test]
fn type_array() {
    let t = parse_type_via_alias("[u8; 16]");
    assert!(matches!(t.kind, TypeKind::Array { .. }));
}

#[test]
fn type_self_keyword() {
    let t = parse_type_via_alias("Self");
    assert!(matches!(t.kind, TypeKind::SelfType));
}

// ---------------------------------------------------------------------------
// Expression literals and atoms
// ---------------------------------------------------------------------------

#[test]
fn expr_int_decimal() {
    let e = parse_expr_ok("42");
    match e.kind {
        ExprKind::Int(i) => {
            assert_eq!(i.raw, "42");
            assert_eq!(i.base, IntBase::Dec);
        }
        _ => panic!(),
    }
}

#[test]
fn expr_int_hex_with_suffix() {
    let e = parse_expr_ok("0xFFu32");
    match e.kind {
        ExprKind::Int(i) => {
            assert_eq!(i.base, IntBase::Hex);
            assert_eq!(i.suffix.as_deref(), Some("u32"));
            assert_eq!(i.raw, "FF");
        }
        _ => panic!(),
    }
}

#[test]
fn expr_float_with_exp() {
    let e = parse_expr_ok("2.5e-3");
    assert!(matches!(e.kind, ExprKind::Float(_)));
}

#[test]
fn expr_bool_and_null() {
    assert!(matches!(parse_expr_ok("true").kind, ExprKind::Bool(true)));
    assert!(matches!(parse_expr_ok("false").kind, ExprKind::Bool(false)));
    assert!(matches!(parse_expr_ok("null").kind, ExprKind::Null));
}

#[test]
fn expr_char() {
    assert!(matches!(parse_expr_ok("'a'").kind, ExprKind::Char(_)));
}

#[test]
fn expr_self() {
    assert!(matches!(parse_expr_ok("self").kind, ExprKind::SelfExpr));
}

#[test]
fn expr_ident() {
    match parse_expr_ok("foo").kind {
        ExprKind::Ident(i) => assert_eq!(i.name, "foo"),
        _ => panic!(),
    }
}

#[test]
fn expr_list() {
    let e = parse_expr_ok("[1, 2, 3]");
    match e.kind {
        ExprKind::List(xs) => assert_eq!(xs.len(), 3),
        _ => panic!(),
    }
}

#[test]
fn expr_empty_list() {
    let e = parse_expr_ok("[]");
    assert!(matches!(e.kind, ExprKind::List(_)));
}

#[test]
fn expr_paren_vs_tuple() {
    assert!(matches!(parse_expr_ok("(1)").kind, ExprKind::Paren(_)));
    let e = parse_expr_ok("(1, 2)");
    match e.kind {
        ExprKind::Tuple(xs) => assert_eq!(xs.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn expr_three_tuple() {
    let e = parse_expr_ok("(1, 2, 3)");
    match e.kind {
        ExprKind::Tuple(xs) => assert_eq!(xs.len(), 3),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

#[test]
fn string_simple() {
    let e = parse_expr_ok(r#""hello""#);
    match e.kind {
        ExprKind::Str(s) => {
            assert_eq!(s.parts.len(), 1);
            match &s.parts[0] {
                StringPart::Text { text, .. } => assert_eq!(text, "hello"),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn string_with_dollar_ident() {
    let e = parse_expr_ok(r#""Hello, $name!""#);
    match e.kind {
        ExprKind::Str(s) => {
            assert_eq!(s.parts.len(), 3);
            match &s.parts[1] {
                StringPart::Ident(id) => assert_eq!(id.name, "name"),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn string_with_interp_expr() {
    let e = parse_expr_ok(r#""age: ${u.age + 1}!""#);
    match e.kind {
        ExprKind::Str(s) => {
            assert_eq!(s.parts.len(), 3);
            match &s.parts[1] {
                StringPart::Expr(_) => {}
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn string_with_nested_string_in_interp() {
    let e = parse_expr_ok(r#""${ "inner" }""#);
    match e.kind {
        ExprKind::Str(_) => {}
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Operator precedence and associativity
// ---------------------------------------------------------------------------

fn binop(e: &Expr) -> (BinaryOp, &Expr, &Expr) {
    match &e.kind {
        ExprKind::Binary {
            op, left, right, ..
        } => (*op, left, right),
        _ => panic!("not a binary"),
    }
}

#[test]
fn precedence_mul_over_add() {
    let e = parse_expr_ok("1 + 2 * 3");
    let (top, l, r) = binop(&e);
    assert_eq!(top, BinaryOp::Add);
    assert!(matches!(l.kind, ExprKind::Int(_)));
    let (rop, _, _) = binop(r);
    assert_eq!(rop, BinaryOp::Mul);
}

#[test]
fn precedence_shift_below_arith() {
    // `1 + 2 << 3` ≡ `(1 + 2) << 3`
    let e = parse_expr_ok("1 + 2 << 3");
    let (top, l, _) = binop(&e);
    assert_eq!(top, BinaryOp::Shl);
    let (lop, _, _) = binop(l);
    assert_eq!(lop, BinaryOp::Add);
}

#[test]
fn precedence_bitor_below_compare_below_logical() {
    // `a || b && c == d | e` parses with `||` outermost.
    let e = parse_expr_ok("a || b && c == d | e");
    let (top, _, _) = binop(&e);
    assert_eq!(top, BinaryOp::Or);
}

#[test]
fn precedence_cast_above_arith() {
    // `x + y as i32` ≡ `x + (y as i32)`
    let e = parse_expr_ok("x + y as i32");
    let (top, _, r) = binop(&e);
    assert_eq!(top, BinaryOp::Add);
    assert!(matches!(r.kind, ExprKind::Cast { .. }));
}

#[test]
fn non_associative_eq_chain_is_error() {
    let (_, errs, _) = parse_src("var x = a == b == c;");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ParseErrorKind::NonAssociativeChain { .. })),
        "got: {errs:?}"
    );
}

#[test]
fn non_associative_lt_chain_is_error() {
    let (_, errs, _) = parse_src("var x = a < b < c;");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ParseErrorKind::NonAssociativeChain { .. })),
        "got: {errs:?}"
    );
}

#[test]
fn unary_minus_and_not() {
    let e = parse_expr_ok("-!x");
    match e.kind {
        ExprKind::Unary { op, operand, .. } => {
            assert_eq!(op, UnaryOp::Neg);
            assert!(matches!(
                operand.kind,
                ExprKind::Unary {
                    op: UnaryOp::Not,
                    ..
                }
            ));
        }
        _ => panic!(),
    }
}

#[test]
fn unary_bitnot() {
    let e = parse_expr_ok("~x");
    assert!(matches!(
        e.kind,
        ExprKind::Unary {
            op: UnaryOp::BitNot,
            ..
        }
    ));
}

#[test]
fn parens_override_precedence() {
    let e = parse_expr_ok("(1 + 2) * 3");
    let (top, l, _) = binop(&e);
    assert_eq!(top, BinaryOp::Mul);
    assert!(matches!(l.kind, ExprKind::Paren(_)));
}

// ---------------------------------------------------------------------------
// Postfix forms
// ---------------------------------------------------------------------------

#[test]
fn method_chain() {
    let e = parse_expr_ok("a.b.c");
    match e.kind {
        ExprKind::Field { receiver, name } => {
            assert_eq!(name.name, "c");
            match &receiver.kind {
                ExprKind::Field { name: m, .. } => assert_eq!(m.name, "b"),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn tuple_index_access() {
    let e = parse_expr_ok("p.0");
    match e.kind {
        ExprKind::TupleIndex { index, .. } => assert_eq!(index, 0),
        _ => panic!(),
    }
}

#[test]
fn call_with_args() {
    let e = parse_expr_ok("f(1, 2, 3)");
    match e.kind {
        ExprKind::Call { args, .. } => assert_eq!(args.len(), 3),
        _ => panic!(),
    }
}

#[test]
fn generic_call() {
    let e = parse_expr_ok("id<i64>(42)");
    match e.kind {
        ExprKind::Call { generics, args, .. } => {
            assert_eq!(generics.len(), 1);
            assert_eq!(args.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn generic_call_with_double_close() {
    let e = parse_expr_ok("Map.new<str, List<i64>>()");
    match e.kind {
        ExprKind::Call {
            callee, generics, ..
        } => {
            assert_eq!(generics.len(), 2);
            assert!(matches!(callee.kind, ExprKind::Field { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn lt_used_as_compare_when_no_call_follows() {
    // `a < b > c` should NOT be parsed as a generic call.
    let (_, errs, _) = parse_src("var x = (a < b) > c;");
    assert!(errs.is_empty());
}

#[test]
fn index_expr() {
    let e = parse_expr_ok("arr[i + 1]");
    match e.kind {
        ExprKind::Index { receiver, index } => {
            assert!(matches!(receiver.kind, ExprKind::Ident(_)));
            assert!(matches!(index.kind, ExprKind::Binary { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn try_operator() {
    let e = parse_expr_ok("foo()?");
    assert!(matches!(e.kind, ExprKind::Try { .. }));
}

#[test]
fn ref_and_deref() {
    let e = parse_expr_ok("&x");
    assert!(matches!(e.kind, ExprKind::Ref { .. }));
    let e = parse_expr_ok("*p");
    assert!(matches!(e.kind, ExprKind::Deref { .. }));
}

#[test]
fn await_expression() {
    let e = parse_expr_ok("await fut");
    assert!(matches!(e.kind, ExprKind::Await { .. }));
}

// ---------------------------------------------------------------------------
// Struct literals
// ---------------------------------------------------------------------------

#[test]
fn struct_literal_named_fields() {
    let e = parse_expr_ok("Person { name: \"a\", age: 1 }");
    match e.kind {
        ExprKind::StructLit { fields, .. } => assert_eq!(fields.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn struct_literal_shorthand() {
    let e = parse_expr_ok("Person { name, age }");
    match e.kind {
        ExprKind::StructLit { fields, .. } => {
            assert!(fields.iter().all(|f| f.value.is_none()));
        }
        _ => panic!(),
    }
}

#[test]
fn struct_literal_spread() {
    let e = parse_expr_ok("Person { ..base, age: 99 }");
    match e.kind {
        ExprKind::StructLit { spread, fields, .. } => {
            assert!(spread.is_some());
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name.name, "age");
        }
        _ => panic!(),
    }
}

#[test]
fn struct_literal_spread_trailing() {
    let e = parse_expr_ok("Person { age: 99, ..base }");
    match e.kind {
        ExprKind::StructLit { spread, fields, .. } => {
            assert!(spread.is_some());
            assert_eq!(fields.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn generic_struct_literal() {
    let e = parse_expr_ok("Box<i64> { value: 42 }");
    match e.kind {
        ExprKind::StructLit { path, .. } => assert_eq!(path.generics.len(), 1),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Control-flow expressions
// ---------------------------------------------------------------------------

#[test]
fn if_else_expression() {
    let e = parse_expr_ok("if x > 0 { 1 } else { -1 }");
    match e.kind {
        ExprKind::If { else_branch, .. } => {
            assert!(matches!(else_branch, Some(ElseBranch::Block(_))));
        }
        _ => panic!(),
    }
}

#[test]
fn if_else_if_chain() {
    let e = parse_expr_ok("if a { 1 } else if b { 2 } else { 3 }");
    match e.kind {
        ExprKind::If { else_branch, .. } => {
            assert!(matches!(else_branch, Some(ElseBranch::If(_))));
        }
        _ => panic!(),
    }
}

#[test]
fn match_with_arms_and_guard() {
    let src = "\
match v {
    i64 x if x > 0 => \"pos\",
    i64 => \"int\",
    _ => \"other\",
}";
    let e = parse_expr_ok(src);
    match e.kind {
        ExprKind::Match { arms, .. } => {
            assert_eq!(arms.len(), 3);
            assert!(arms[0].guard.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn loop_with_break_value() {
    let e = parse_expr_ok("loop { break 42; }");
    assert!(matches!(e.kind, ExprKind::Loop(_)));
}

#[test]
fn while_loop() {
    let e = parse_expr_ok("while x > 0 { x = x - 1; }");
    assert!(matches!(e.kind, ExprKind::While { .. }));
}

#[test]
fn for_loop() {
    let e = parse_expr_ok("for x in xs { print(x); }");
    assert!(matches!(e.kind, ExprKind::For { .. }));
}

#[test]
fn for_await_loop() {
    let e = parse_expr_ok("for await x in stream { print(x); }");
    match e.kind {
        ExprKind::For { in_async, .. } => assert!(in_async),
        _ => panic!(),
    }
}

#[test]
fn block_with_trailing_value() {
    let e = parse_expr_ok("{ var x = 1; x + 1 }");
    match e.kind {
        ExprKind::Block(b) => {
            assert_eq!(b.stmts.len(), 1);
            assert!(b.trailing.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn block_without_trailing_value() {
    let e = parse_expr_ok("{ var x = 1; }");
    match e.kind {
        ExprKind::Block(b) => assert!(b.trailing.is_none()),
        _ => panic!(),
    }
}

#[test]
fn async_block() {
    let e = parse_expr_ok("async { 1 }");
    assert!(matches!(e.kind, ExprKind::AsyncBlock(_)));
}

#[test]
fn return_with_value() {
    let e = parse_expr_ok("return 42");
    match e.kind {
        ExprKind::Return(v) => assert!(v.is_some()),
        _ => panic!(),
    }
}

#[test]
fn return_without_value_in_block() {
    let m = parse_ok("function f() { return; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let body = f.body.as_ref().unwrap();
            assert_eq!(body.stmts.len(), 1);
        }
        _ => panic!(),
    }
}

#[test]
fn continue_keyword() {
    let m = parse_ok("function f() { loop { continue; } }");
    match &m.items[0].kind {
        ItemKind::Function(_) => {}
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Closures and anonymous functions
// ---------------------------------------------------------------------------

#[test]
fn arrow_closure_simple() {
    let e = parse_expr_ok("(x) => x * 2");
    match e.kind {
        ExprKind::Closure { params, .. } => assert_eq!(params.len(), 1),
        _ => panic!("got {:?}", e.kind),
    }
}

#[test]
fn arrow_closure_block_body() {
    let e = parse_expr_ok("(x) => { var y = x + 1; y * y }");
    match e.kind {
        ExprKind::Closure { body, .. } => {
            assert!(matches!(body.kind, ExprKind::Block(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn arrow_closure_zero_args() {
    let e = parse_expr_ok("() => 42");
    match e.kind {
        ExprKind::Closure { params, .. } => assert!(params.is_empty()),
        _ => panic!(),
    }
}

#[test]
fn arrow_closure_async() {
    let e = parse_expr_ok("(x) async => x");
    match e.kind {
        ExprKind::Closure { is_async, .. } => assert!(is_async),
        _ => panic!(),
    }
}

#[test]
fn arrow_closure_with_return_type() {
    let e = parse_expr_ok("(x: i32): i32 => x + 1");
    match e.kind {
        ExprKind::Closure { return_type, .. } => assert!(return_type.is_some()),
        _ => panic!(),
    }
}

#[test]
fn anonymous_function_expression() {
    let e = parse_expr_ok("function(x: i32): i32 { x * 2 }");
    assert!(matches!(e.kind, ExprKind::AnonFn(_)));
}

#[test]
fn trailing_closure_on_method() {
    let e = parse_expr_ok("xs.map { x => x * 2 }");
    match e.kind {
        ExprKind::Call {
            trailing_closure, ..
        } => {
            let tc = trailing_closure.expect("expected trailing closure");
            assert!(matches!(tc.kind, ExprKind::Closure { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn trailing_closure_async_no_params() {
    // `Thread.spawn { async => … }` (docs/20 §1): a parameterless async
    // trailing closure. The `async` keyword (no params) precedes the `=>`.
    let e = parse_expr_ok("Thread.spawn { async => 7 }");
    match e.kind {
        ExprKind::Call {
            trailing_closure, ..
        } => {
            let tc = trailing_closure.expect("expected trailing closure");
            match tc.kind {
                ExprKind::Closure {
                    is_async, params, ..
                } => {
                    assert!(is_async, "trailing `{{ async => … }}` is an async closure");
                    assert!(params.is_empty());
                }
                _ => panic!("expected a closure"),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn trailing_closure_async_with_params() {
    // `s.lock { c async => … }` (docs/20 §4): an async trailing closure with a
    // parameter. The `async` keyword sits between the params and the `=>`.
    let e = parse_expr_ok("s.lock { c async => c }");
    match e.kind {
        ExprKind::Call {
            trailing_closure, ..
        } => {
            let tc = trailing_closure.expect("expected trailing closure");
            match tc.kind {
                ExprKind::Closure {
                    is_async, params, ..
                } => {
                    assert!(is_async);
                    assert_eq!(params.len(), 1);
                }
                _ => panic!("expected a closure"),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn trailing_closure_with_inner_async_block_not_async_closure() {
    // A trailing closure whose first statement is an `async { … }` block must
    // NOT be misparsed as an async closure header: `{ async { … } }` is a
    // closure with an async-block body, not `{ async => … }`.
    let e = parse_expr_ok("Thread.spawn { async { 1 } }");
    match e.kind {
        ExprKind::Call {
            trailing_closure, ..
        } => {
            let tc = trailing_closure.expect("expected trailing closure");
            match tc.kind {
                ExprKind::Closure { is_async, .. } => assert!(!is_async),
                _ => panic!("expected a closure"),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn trailing_closure_after_call_args() {
    let e = parse_expr_ok("xs.fold(0) { acc, n => acc + n }");
    match e.kind {
        ExprKind::Call {
            trailing_closure,
            args,
            ..
        } => {
            assert_eq!(args.len(), 1);
            assert!(trailing_closure.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn trailing_closure_implicit_it() {
    let e = parse_expr_ok("xs.map { it * 2 }");
    match e.kind {
        ExprKind::Call {
            trailing_closure: Some(tc),
            ..
        } => match tc.kind {
            ExprKind::Closure { params, .. } => assert!(params.is_empty()),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Statements and assignment
// ---------------------------------------------------------------------------

#[test]
fn assignment_statement() {
    let m = parse_ok("function f() { var x = 1; x = 2; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let body = f.body.as_ref().unwrap();
            assert!(matches!(body.stmts[1].kind, StmtKind::Assign { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn nested_var_with_pattern() {
    let m = parse_ok("function f() { var (a, b) = (1, 2); }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let body = f.body.as_ref().unwrap();
            match &body.stmts[0].kind {
                StmtKind::Var(v) => assert!(matches!(v.pattern.kind, PatternKind::Tuple { .. })),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn expression_statement_block_form_no_semi() {
    let m = parse_ok("function f() { if x { 1 } else { 2 } var y = 3; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let body = f.body.as_ref().unwrap();
            assert_eq!(body.stmts.len(), 2);
        }
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

fn parse_match_arm_pattern(src: &str) -> Pattern {
    let wrapper = format!("function f() {{ match x {{ {src} => 1, _ => 0 }} }}");
    let m = parse_ok(&wrapper);
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let body = f.body.as_ref().unwrap();
            let trailing = body.trailing.as_ref().unwrap();
            match &trailing.kind {
                ExprKind::Match { arms, .. } => arms[0].pattern.clone(),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_wildcard() {
    assert!(matches!(
        parse_match_arm_pattern("_").kind,
        PatternKind::Wildcard
    ));
}

#[test]
fn pattern_literal_int() {
    let p = parse_match_arm_pattern("42");
    assert!(matches!(p.kind, PatternKind::Literal(_)));
}

#[test]
fn pattern_literal_string() {
    let p = parse_match_arm_pattern("\"hello\"");
    assert!(matches!(p.kind, PatternKind::Literal(_)));
}

#[test]
fn pattern_negative_literal() {
    let p = parse_match_arm_pattern("-1");
    assert!(matches!(p.kind, PatternKind::Literal(_)));
}

#[test]
fn pattern_binding() {
    match parse_match_arm_pattern("name").kind {
        PatternKind::Binding(i) => assert_eq!(i.name, "name"),
        other => panic!("got {:?}", other),
    }
}

#[test]
fn pattern_type_binding() {
    let p = parse_match_arm_pattern("i64 n");
    match p.kind {
        PatternKind::TypeBinding { ty, binding } => {
            assert!(matches!(ty.kind, TypeKind::Named { .. }));
            assert_eq!(binding.unwrap().name, "n");
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_primitive_only() {
    let p = parse_match_arm_pattern("i64");
    match p.kind {
        PatternKind::TypeBinding { binding, .. } => assert!(binding.is_none()),
        _ => panic!(),
    }
}

#[test]
fn pattern_unit_struct() {
    // capitalized name = unit-path pattern, not a binding.
    let p = parse_match_arm_pattern("Red");
    assert!(matches!(p.kind, PatternKind::UnitPath(_)));
}

#[test]
fn pattern_tuple_struct() {
    let p = parse_match_arm_pattern("Some(a, b)");
    match p.kind {
        PatternKind::TupleStruct { fields, .. } => assert_eq!(fields.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn pattern_tuple_struct_with_rest() {
    let p = parse_match_arm_pattern("Many(a, ..rest)");
    match p.kind {
        PatternKind::TupleStruct { rest, .. } => {
            assert!(rest.is_some());
            assert_eq!(rest.unwrap().name.unwrap().name, "rest");
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_record_struct() {
    let p = parse_match_arm_pattern("Person { name, age }");
    match p.kind {
        PatternKind::RecordStruct {
            fields, has_rest, ..
        } => {
            assert_eq!(fields.len(), 2);
            assert!(!has_rest);
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_record_struct_with_rest() {
    let p = parse_match_arm_pattern("Person { name, .. }");
    match p.kind {
        PatternKind::RecordStruct { has_rest, .. } => assert!(has_rest),
        _ => panic!(),
    }
}

#[test]
fn pattern_tuple() {
    let p = parse_match_arm_pattern("(a, b)");
    match p.kind {
        PatternKind::Tuple { elems, .. } => assert_eq!(elems.len(), 2),
        _ => panic!(),
    }
}

#[test]
fn pattern_tuple_with_rest_in_middle() {
    let p = parse_match_arm_pattern("(a, ..mid, b)");
    match p.kind {
        PatternKind::Tuple { rest, .. } => {
            let (idx, r) = rest.unwrap();
            assert_eq!(idx, 1);
            assert_eq!(r.name.as_ref().unwrap().name, "mid");
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_list_with_head_tail() {
    let p = parse_match_arm_pattern("[head, ..tail]");
    match p.kind {
        PatternKind::List { elems, rest } => {
            assert_eq!(elems.len(), 1);
            let (idx, _) = rest.unwrap();
            assert_eq!(idx, 1);
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_empty_list() {
    let p = parse_match_arm_pattern("[]");
    match p.kind {
        PatternKind::List { elems, rest } => {
            assert!(elems.is_empty());
            assert!(rest.is_none());
        }
        _ => panic!(),
    }
}

#[test]
fn pattern_or_pattern() {
    let p = parse_match_arm_pattern("1 | 2 | 3");
    match p.kind {
        PatternKind::Or(alts) => assert_eq!(alts.len(), 3),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Headers in if/while/for don't accept struct literals or trailing closures
// ---------------------------------------------------------------------------

#[test]
fn if_header_does_not_eat_struct_lit_brace() {
    // `if x { 1 }` — `x` is the condition; `{ 1 }` is the body.
    let e = parse_expr_ok("if cond { 1 } else { 2 }");
    match e.kind {
        ExprKind::If { cond, .. } => {
            assert!(matches!(cond.kind, ExprKind::Ident(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn for_header_requires_parens_for_struct_lit() {
    let e = parse_expr_ok("for n in (Range { current: 0, end: 5 }) { n }");
    match e.kind {
        ExprKind::For { iter, .. } => {
            assert!(matches!(iter.kind, ExprKind::Paren(_)));
        }
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Spans
// ---------------------------------------------------------------------------

#[test]
fn item_span_covers_pub_through_semi() {
    let src = "pub var x: i64 = 42;";
    let (m, _, _) = parse_src(src);
    let item = &m.items[0];
    assert_eq!(slice(src, item.span), src);
}

#[test]
fn function_item_span_covers_signature_and_body() {
    let src = "function add(a: i64, b: i64): i64 { a + b }";
    let (m, _, _) = parse_src(src);
    let item = &m.items[0];
    assert_eq!(slice(src, item.span), src);
}

#[test]
fn binary_expr_span_covers_lhs_to_rhs() {
    let src = "var v = 1 + 2 * 3;";
    let (m, _, _) = parse_src(src);
    match &m.items[0].kind {
        ItemKind::Var(v) => assert_eq!(slice(src, v.init.span), "1 + 2 * 3"),
        _ => panic!(),
    }
}

#[test]
fn block_span_includes_braces() {
    let src = "function f() { 1 + 1 }";
    let (m, _, _) = parse_src(src);
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let b = f.body.as_ref().unwrap();
            assert_eq!(slice(src, b.span), "{ 1 + 1 }");
        }
        _ => panic!(),
    }
}

#[test]
fn struct_literal_span_covers_path_through_brace() {
    let src = "var p = Point { x: 1, y: 2 };";
    let (m, _, _) = parse_src(src);
    match &m.items[0].kind {
        ItemKind::Var(v) => assert_eq!(slice(src, v.init.span), "Point { x: 1, y: 2 }"),
        _ => panic!(),
    }
}

#[test]
fn ident_span_is_exact() {
    let src = "var hello = world;";
    let (m, _, _) = parse_src(src);
    match &m.items[0].kind {
        ItemKind::Var(v) => {
            assert_eq!(slice(src, v.name.span), "hello");
            match &v.init.kind {
                ExprKind::Ident(i) => assert_eq!(slice(src, i.span), "world"),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn match_arm_span_covers_pattern_through_body() {
    let src = "var v = match x { 1 => \"a\", _ => \"b\" };";
    let (m, _, _) = parse_src(src);
    match &m.items[0].kind {
        ItemKind::Var(v) => match &v.init.kind {
            ExprKind::Match { arms, .. } => {
                assert_eq!(slice(src, arms[0].span), "1 => \"a\"");
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn string_literal_span_includes_quotes() {
    let src = r#"var s = "hello";"#;
    let (m, _, _) = parse_src(src);
    match &m.items[0].kind {
        ItemKind::Var(v) => assert_eq!(slice(src, v.init.span), "\"hello\""),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Error recovery
// ---------------------------------------------------------------------------

#[test]
fn missing_semicolon_reports_error_but_continues() {
    let (m, errs, _) = parse_src("var x = 1 var y = 2;");
    assert!(!errs.is_empty(), "expected errors");
    // We can still see at least one item.
    assert!(!m.items.is_empty());
}

#[test]
fn unknown_token_reports_expected_expression() {
    let (_, errs, _) = parse_src("var x = ;");
    assert!(errs
        .iter()
        .any(|e| matches!(&e.kind, ParseErrorKind::Expected { expected, .. } if expected.iter().any(|s| s.contains("expression")))));
}

#[test]
fn missing_function_body_reports_error() {
    let (_, errs, _) = parse_src("function f()");
    assert!(!errs.is_empty());
}

#[test]
fn missing_close_paren_reports_error() {
    let (_, errs, _) = parse_src("function f(x: i64 { }");
    assert!(!errs.is_empty());
}

// ---------------------------------------------------------------------------
// Visibility on items
// ---------------------------------------------------------------------------

#[test]
fn pub_on_each_item_kind() {
    let src = "\
pub function f() {}
pub struct S { pub a: i64 }
pub interface I { function m(self); }
pub type A = i64;
pub mod m;
pub import \"x\";
pub var v: i64 = 0;
";
    let m = parse_ok(src);
    for item in &m.items {
        assert!(item.visibility.is_public(), "{:?}", item.kind);
    }
}

// ---------------------------------------------------------------------------
// Misc / smoke tests
// ---------------------------------------------------------------------------

#[test]
fn complex_program_parses_clean() {
    let src = "\
//! crate root

import { println } from \"std:io\";

pub struct Point { pub x: f64, pub y: f64 }

extend Point {
    function origin(): Point { Point { x: 0.0, y: 0.0 } }
    function magnitude(self): f64 { self.x * self.x + self.y * self.y }
}

pub function distance(a: Point, b: Point): f64 {
    var dx = a.x - b.x;
    var dy = a.y - b.y;
    (dx * dx + dy * dy)
}

pub function main() {
    var p = Point.origin();
    var q = Point { x: 3.0, y: 4.0 };
    var d = distance(p, q);
    if d > 0.0 {
        Println(\"d=$d\");
    } else {
        Println(\"zero\");
    }
}
";
    let (_, errs, _) = parse_src(src);
    assert!(errs.is_empty(), "errors: {errs:#?}");
}

// ===========================================================================
// Verification pass 2 — edge cases discovered after initial implementation
// ===========================================================================

#[test]
fn triple_nested_generic_with_shr_close() {
    // `Foo<Bar<Baz<T>>>` — the `>>>` is `>` + `>>` from the lexer's view.
    let t = parse_type_via_alias("Foo<Bar<Baz<T>>>");
    match t.kind {
        TypeKind::Named { generics, .. } => {
            assert_eq!(generics.len(), 1);
            match &generics[0].kind {
                TypeKind::Named { generics: g2, .. } => assert_eq!(g2.len(), 1),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn empty_struct_literal() {
    let e = parse_expr_ok("Empty {}");
    match e.kind {
        ExprKind::StructLit { fields, spread, .. } => {
            assert!(fields.is_empty());
            assert!(spread.is_none());
        }
        _ => panic!(),
    }
}

#[test]
fn nested_struct_literal_in_value() {
    let e = parse_expr_ok("Outer { inner: Inner { x: 1 } }");
    match e.kind {
        ExprKind::StructLit { fields, .. } => {
            assert_eq!(fields.len(), 1);
            assert!(matches!(
                fields[0].value.as_ref().unwrap().kind,
                ExprKind::StructLit { .. }
            ));
        }
        _ => panic!(),
    }
}

#[test]
fn method_call_chain_with_generics() {
    let e = parse_expr_ok("obj.foo<i64>().bar<str>()");
    // Outermost is the second call.
    match e.kind {
        ExprKind::Call {
            callee, generics, ..
        } => {
            assert_eq!(generics.len(), 1);
            assert!(matches!(callee.kind, ExprKind::Field { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn assignment_to_field() {
    let m = parse_ok("function f() { x.y = 1; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let b = f.body.as_ref().unwrap();
            match &b.stmts[0].kind {
                StmtKind::Assign { target, .. } => {
                    assert!(matches!(target.kind, ExprKind::Field { .. }))
                }
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn assignment_to_index() {
    let m = parse_ok("function f() { arr[i] = v; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let b = f.body.as_ref().unwrap();
            match &b.stmts[0].kind {
                StmtKind::Assign { target, .. } => {
                    assert!(matches!(target.kind, ExprKind::Index { .. }))
                }
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn assignment_to_deref() {
    let m = parse_ok("function f() { *p = 1; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let b = f.body.as_ref().unwrap();
            match &b.stmts[0].kind {
                StmtKind::Assign { target, .. } => {
                    assert!(matches!(target.kind, ExprKind::Deref { .. }))
                }
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn if_as_var_initializer() {
    let m = parse_ok("function f() { var x = if c { 1 } else { 2 }; }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let b = f.body.as_ref().unwrap();
            match &b.stmts[0].kind {
                StmtKind::Var(v) => assert!(matches!(v.init.kind, ExprKind::If { .. })),
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn match_arm_with_block_body() {
    let e = parse_expr_ok("match x { 1 => { var y = 2; y + 1 }, _ => 0 }");
    match e.kind {
        ExprKind::Match { arms, .. } => {
            assert!(matches!(arms[0].body.kind, ExprKind::Block(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn char_pattern_with_escape() {
    let p = parse_match_arm_pattern("'\\n'");
    assert!(matches!(p.kind, PatternKind::Literal(_)));
}

#[test]
fn nested_tuple_pattern() {
    let p = parse_match_arm_pattern("((a, b), c)");
    match p.kind {
        PatternKind::Tuple { elems, .. } => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(elems[0].kind, PatternKind::Tuple { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn list_pattern_init_last() {
    let p = parse_match_arm_pattern("[..init, last]");
    match p.kind {
        PatternKind::List { elems, rest } => {
            assert_eq!(elems.len(), 1);
            let (idx, _) = rest.unwrap();
            assert_eq!(idx, 0);
        }
        _ => panic!(),
    }
}

#[test]
fn list_pattern_a_mid_z() {
    let p = parse_match_arm_pattern("[a, ..mid, z]");
    match p.kind {
        PatternKind::List { elems, rest } => {
            assert_eq!(elems.len(), 2);
            let (idx, _) = rest.unwrap();
            assert_eq!(idx, 1);
        }
        _ => panic!(),
    }
}

#[test]
fn empty_match_parses() {
    let e = parse_expr_ok("match x {}");
    match e.kind {
        ExprKind::Match { arms, .. } => assert!(arms.is_empty()),
        _ => panic!(),
    }
}

#[test]
fn empty_if_body() {
    let e = parse_expr_ok("if c {} else {}");
    assert!(matches!(e.kind, ExprKind::If { .. }));
}

#[test]
fn nested_closures() {
    let e = parse_expr_ok("(x) => (y) => x + y");
    match e.kind {
        ExprKind::Closure { body, .. } => {
            assert!(matches!(body.kind, ExprKind::Closure { .. }))
        }
        _ => panic!(),
    }
}

#[test]
fn closure_with_no_params_typed_return() {
    let e = parse_expr_ok("(): i64 => 42");
    match e.kind {
        ExprKind::Closure {
            params,
            return_type,
            ..
        } => {
            assert!(params.is_empty());
            assert!(return_type.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn cast_chain() {
    let e = parse_expr_ok("x as i32 as i64");
    // Left-associative: (x as i32) as i64
    match e.kind {
        ExprKind::Cast { expr, .. } => {
            assert!(matches!(expr.kind, ExprKind::Cast { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn is_operator() {
    let e = parse_expr_ok("v is Foo");
    match e.kind {
        ExprKind::Cast { op, .. } => assert_eq!(op, CastOp::Is),
        _ => panic!(),
    }
}

#[test]
fn pointer_type_in_tuple() {
    let t = parse_type_via_alias("(*i64, *u8)");
    match t.kind {
        TypeKind::Tuple(ts) => {
            assert_eq!(ts.len(), 2);
            assert!(matches!(ts[0].kind, TypeKind::Pointer(_)));
        }
        _ => panic!(),
    }
}

#[test]
fn higher_order_function_type() {
    let t = parse_type_via_alias("(i64) => (i64) => i64");
    // (i64) => ((i64) => i64) — right-recursive function type.
    match t.kind {
        TypeKind::Function { ret, .. } => {
            assert!(matches!(ret.kind, TypeKind::Function { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn underscore_in_assign_lhs() {
    let m = parse_ok("function f() { _ = expensive(); }");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let b = f.body.as_ref().unwrap();
            match &b.stmts[0].kind {
                StmtKind::Assign { target, .. } => {
                    assert!(matches!(target.kind, ExprKind::Underscore))
                }
                _ => panic!(),
            }
        }
        _ => panic!(),
    }
}

#[test]
fn return_in_block_arm() {
    let m = parse_ok("function f(): i64 { if x { return 1 } else { 2 } }");
    match &m.items[0].kind {
        ItemKind::Function(_) => {}
        _ => panic!(),
    }
}

#[test]
fn unary_minus_with_method_call() {
    // `-x.y()` ≡ `-(x.y())` (postfix binds tighter than unary)
    let e = parse_expr_ok("-x.method()");
    match e.kind {
        ExprKind::Unary { operand, .. } => {
            assert!(matches!(operand.kind, ExprKind::Call { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn try_propagates_after_await() {
    // `(await fut)?`
    let e = parse_expr_ok("(await fut)?");
    assert!(matches!(e.kind, ExprKind::Try { .. }));
}

#[test]
fn parens_grouping_of_lt_compare() {
    // The docs say to use parens to force comparison reading.
    let e = parse_expr_ok("(a < b)");
    match e.kind {
        ExprKind::Paren(inner) => {
            assert!(matches!(
                inner.kind,
                ExprKind::Binary {
                    op: BinaryOp::Lt,
                    ..
                }
            ))
        }
        _ => panic!(),
    }
}

#[test]
fn struct_method_call_on_literal() {
    let e = parse_expr_ok("Point { x: 1, y: 2 }.magnitude()");
    match e.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Field { receiver, .. } => {
                assert!(matches!(receiver.kind, ExprKind::StructLit { .. }))
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn very_deep_method_chain() {
    let e = parse_expr_ok("a.b.c.d.e.f");
    // Each step is a Field; outermost is `.f`.
    match e.kind {
        ExprKind::Field { name, .. } => assert_eq!(name.name, "f"),
        _ => panic!(),
    }
}

#[test]
fn block_with_multiple_statements_and_trailing() {
    let e = parse_expr_ok("{ var a = 1; var b = 2; a + b }");
    match e.kind {
        ExprKind::Block(b) => {
            assert_eq!(b.stmts.len(), 2);
            assert!(b.trailing.is_some());
        }
        _ => panic!(),
    }
}

#[test]
fn extend_member_can_have_pub() {
    let m = parse_ok("extend P { pub function foo(self): i64 { 0 } }");
    match &m.items[0].kind {
        ItemKind::Extend(e) => assert!(e.members[0].visibility.is_public()),
        _ => panic!(),
    }
}

#[test]
fn struct_field_doc_attaches() {
    let m = parse_ok("struct P {\n    /// doc\n    pub x: i64,\n}");
    match &m.items[0].kind {
        ItemKind::Struct(s) => match &s.kind {
            StructKind::Record(fs) => assert_eq!(fs[0].docs.len(), 1),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn interface_method_decl_terminated_by_semi() {
    let m = parse_ok("interface I { function m(self): i64; }");
    match &m.items[0].kind {
        ItemKind::Interface(i) => assert!(i.members[0].default_body.is_none()),
        _ => panic!(),
    }
}

#[test]
fn extern_function_with_body() {
    let m = parse_ok("extern function exported(): i32 { 42 }");
    match &m.items[0].kind {
        ItemKind::Extern(ExternItem::Function(f)) => assert!(f.body.is_some()),
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Span-correctness — every node's slice round-trips through the source.
// ---------------------------------------------------------------------------

#[test]
fn span_round_trip_var_item() {
    let src = "var x: i64 = 42;";
    let (m, _, _) = parse_src(src);
    assert_eq!(slice(src, m.items[0].span), src);
}

#[test]
fn span_round_trip_function_no_body() {
    let src = "extern function malloc(n: u64): *u8;";
    let (m, _, _) = parse_src(src);
    assert_eq!(slice(src, m.items[0].span), src);
}

#[test]
fn span_round_trip_struct_record() {
    let src = "pub struct P { pub x: i64, pub y: i64 }";
    let (m, _, _) = parse_src(src);
    assert_eq!(slice(src, m.items[0].span), src);
}

#[test]
fn span_round_trip_interface() {
    let src = "interface I { function f(self): i64; }";
    let (m, _, _) = parse_src(src);
    assert_eq!(slice(src, m.items[0].span), src);
}

#[test]
fn span_round_trip_extend() {
    let src = "extend<T> Wrapper<T> { function get(self): T { self.value } }";
    let (m, _, _) = parse_src(src);
    assert_eq!(slice(src, m.items[0].span), src);
}

#[test]
fn span_round_trip_import_named() {
    let src = "import { a, b as c } from \"lib\";";
    let (m, _, _) = parse_src(src);
    assert_eq!(slice(src, m.items[0].span), src);
}

#[test]
fn span_round_trip_for_loop() {
    let src = "for x in xs { print(x); }";
    let e = parse_expr_ok(src);
    // The wrapper adds a `var __x = `; we re-extract the slice from the
    // parser-side source. We can at least check it ends with `}`.
    let _ = e;
    assert!(slice(&format!("var __x = {src};"), e.span).ends_with("}"));
}

#[test]
fn span_round_trip_cast_chain() {
    let src = "x as i32 as i64";
    let wrap = format!("var __x = {src};");
    let (m, _, _) = parse_src(&wrap);
    match &m.items[0].kind {
        ItemKind::Var(v) => assert_eq!(slice(&wrap, v.init.span), src),
        _ => panic!(),
    }
}

#[test]
fn span_round_trip_match_arm_body_block() {
    let src = "match x { 1 => { 1 + 1 }, _ => 0 }";
    let wrap = format!("var __x = {src};");
    let (m, _, _) = parse_src(&wrap);
    match &m.items[0].kind {
        ItemKind::Var(v) => match &v.init.kind {
            ExprKind::Match { arms, .. } => {
                assert_eq!(slice(&wrap, arms[0].body.span), "{ 1 + 1 }");
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

// ---------------------------------------------------------------------------
// Error recovery / locality
// ---------------------------------------------------------------------------

#[test]
fn parser_continues_past_bad_item_to_next() {
    let src = "fnubar; function ok() {}";
    let (m, errs, _) = parse_src(src);
    assert!(!errs.is_empty());
    // The good item should still be parsed.
    assert!(
        m.items
            .iter()
            .any(|it| matches!(&it.kind, ItemKind::Function(f) if f.name.name == "ok"))
    );
}

#[test]
fn dangling_else_without_if_is_an_error() {
    let (_, errs, _) = parse_src("function f() { else { 1 } }");
    assert!(!errs.is_empty());
}

#[test]
fn non_assoc_lt_gt_chain_errors() {
    let (_, errs, _) = parse_src("var x = a < b > c;");
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ParseErrorKind::NonAssociativeChain { .. })),
        "got: {errs:?}"
    );
}

#[test]
fn generic_call_with_shr_close_then_call() {
    // `f<Bar<T>>(x)` — the closer is `>>` and the parser must eat one `>`
    // for the inner generic and one for the outer.
    let e = parse_expr_ok("f<Bar<i64>>(x)");
    match e.kind {
        ExprKind::Call { generics, .. } => assert_eq!(generics.len(), 1),
        _ => panic!(),
    }
}

#[test]
fn underscore_prefixed_pattern_is_binding() {
    let p = parse_match_arm_pattern("_unused");
    match p.kind {
        PatternKind::Binding(i) => assert_eq!(i.name, "_unused"),
        other => panic!("got {:?}", other),
    }
}

#[test]
fn record_struct_pattern_in_for_loop() {
    let e = parse_expr_ok("for Person { name, .. } in people { name }");
    match e.kind {
        ExprKind::For { pattern, .. } => {
            assert!(matches!(pattern.kind, PatternKind::RecordStruct { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn nested_trailing_closure() {
    let e = parse_expr_ok("xs.flat_map { x => x.map { it * 2 } }");
    match e.kind {
        ExprKind::Call {
            trailing_closure: Some(tc),
            ..
        } => match tc.kind {
            ExprKind::Closure { body, .. } => match body.kind {
                ExprKind::Block(b) => {
                    let t = b.trailing.expect("trailing");
                    assert!(matches!(t.kind, ExprKind::Call { .. }));
                }
                _ => panic!(),
            },
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn generic_bound_does_not_eat_pipe() {
    // `T: A + B` — bounds are `A` and `B`; `|` is NOT allowed inside a bound.
    // We just verify both bounds were captured.
    let m = parse_ok("function f<T: A + B>() {}");
    match &m.items[0].kind {
        ItemKind::Function(f) => {
            let g = f.generics.as_ref().unwrap();
            assert_eq!(g.params[0].bounds.len(), 2);
        }
        _ => panic!(),
    }
}

#[test]
fn struct_field_with_union_type() {
    let m = parse_ok("struct R { value: A | B | C }");
    match &m.items[0].kind {
        ItemKind::Struct(s) => match &s.kind {
            StructKind::Record(fs) => match &fs[0].ty.kind {
                TypeKind::Union(alts) => assert_eq!(alts.len(), 3),
                _ => panic!(),
            },
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn function_param_with_union_type() {
    let m = parse_ok("function f(x: A | B): A | B { x }");
    match &m.items[0].kind {
        ItemKind::Function(f) => match &f.params[0].kind {
            ParamKind::Normal { ty, .. } => assert!(matches!(ty.kind, TypeKind::Union(_))),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn function_call_with_method_chained() {
    let e = parse_expr_ok("a().b().c()");
    // Outermost is `.c()`.
    match e.kind {
        ExprKind::Call { callee, .. } => match &callee.kind {
            ExprKind::Field { name, .. } => assert_eq!(name.name, "c"),
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn unary_inside_arg_list() {
    let e = parse_expr_ok("f(-1, !flag, ~mask)");
    match e.kind {
        ExprKind::Call { args, .. } => {
            assert_eq!(args.len(), 3);
            assert!(matches!(args[0].kind, ExprKind::Unary { .. }));
        }
        _ => panic!(),
    }
}

#[test]
fn precedence_logical_and_below_bitor() {
    // `a | b && c | d` ≡ `(a | b) && (c | d)`
    let e = parse_expr_ok("a | b && c | d");
    let (top, l, r) = binop(&e);
    assert_eq!(top, BinaryOp::And);
    assert!(matches!(
        l.kind,
        ExprKind::Binary {
            op: BinaryOp::BitOr,
            ..
        }
    ));
    assert!(matches!(
        r.kind,
        ExprKind::Binary {
            op: BinaryOp::BitOr,
            ..
        }
    ));
}

#[test]
fn precedence_compare_below_bitor() {
    // `a == b | c` ≡ `a == (b | c)` — comparison is LOWER than `|` per the
    // precedence table.
    let e = parse_expr_ok("a == b | c");
    let (top, _, r) = binop(&e);
    assert_eq!(top, BinaryOp::Eq);
    assert!(matches!(
        r.kind,
        ExprKind::Binary {
            op: BinaryOp::BitOr,
            ..
        }
    ));
}

#[test]
fn inline_module_with_inner_docs() {
    let src = "mod inner { //! inner mod doc\nvar x = 1; }";
    let m = parse_ok(src);
    match &m.items[0].kind {
        ItemKind::Module(m2) => match &m2.kind {
            ModuleKind::Inline { inner_docs, items } => {
                assert_eq!(inner_docs.len(), 1);
                assert_eq!(items.len(), 1);
            }
            _ => panic!(),
        },
        _ => panic!(),
    }
}

#[test]
fn duplicate_rest_in_tuple_pattern_errors() {
    let src = "var (a, ..r1, b, ..r2) = (1, 2, 3, 4);";
    let wrap = format!("function f() {{ {src} }}");
    let (_, errs, _) = parse_src(&wrap);
    assert!(
        errs.iter()
            .any(|e| matches!(e.kind, ParseErrorKind::DuplicateRestBinding)),
        "got: {errs:?}"
    );
}

#[test]
fn lex_and_parse_full_program_with_all_features() {
    let src = "\
@Derive(Eq, Hash, Clone)
pub struct Person<T> {
    pub name: str,
    pub age: i64,
    pub tag: T,
}

pub interface Greet { function greet(self): str { \"hello\" } }

extend<T: Clone> Person<T>: Greet {
    function greet(self): str { \"hi, $self.name\" }
}

pub type Result<T, E> = T | E;

pub function map<I, O>(xs: List<I>, f: (I) => O): List<O> {
    var out: List<O> = [];
    for x in xs { out.push(f(x)); }
    out
}

pub function main() {
    var ps = [Person { name: \"A\", age: 1, tag: 0 }, Person { name: \"B\", age: 2, tag: 1 }];
    var names = ps.map { p => p.name };
    match names {
        [] => print(\"empty\"),
        [head, ..tail] => print(head),
    }
}
";
    let (_, errs, _) = parse_src(src);
    assert!(errs.is_empty(), "errors:\n{errs:#?}");
}

// ---------------------------------------------------------------------------
// `test "name" { … }` declarations (contextual keyword, docs/23)
// ---------------------------------------------------------------------------

#[test]
fn parses_test_declaration() {
    let m = parse_ok("test \"adds numbers\" { var x = 1 + 2; }");
    assert_eq!(m.items.len(), 1);
    match &m.items[0].kind {
        ItemKind::Test(t) => {
            assert_eq!(t.name, "adds numbers");
            assert_eq!(t.body.stmts.len(), 1);
        }
        other => panic!("expected a test item, got {other:?}"),
    }
}

#[test]
fn test_is_a_contextual_keyword() {
    // `test` used as an ordinary identifier (a variable) still parses.
    let m = parse_ok("function f() { var test = 5; }");
    assert_eq!(m.items.len(), 1);
    assert!(matches!(m.items[0].kind, ItemKind::Function(_)));
}
