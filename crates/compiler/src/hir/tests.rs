//! Stage 1 HIR tests: construct every node and assert its shape and provenance.
//!
//! There is no lowering yet (Stage 2), so these are *structural* tests — they
//! pin down the HIR's public surface, prove every variant is constructible and
//! self-describing (carries its [`Ty`] and [`Span`]), and guard the program
//! container's accessors. Lowering- and codegen-level coverage is added in the
//! later stages; keeping these green ensures the type definitions never drift.

use super::*;
use crate::ids::{DefId, LocalId};
use crate::span::{BytePos, FileId, Span};
use crate::ty::TyCtxt;

/// A distinct span per call so provenance assertions are meaningful.
fn sp(lo: u32, hi: u32) -> Span {
    Span::new(FileId(0), BytePos(lo), BytePos(hi))
}

fn expr(kind: ExprKind, ty: Ty, lo: u32, hi: u32) -> Expr {
    Expr { kind, ty, span: sp(lo, hi) }
}

// ---------------------------------------------------------------------------
// Program container
// ---------------------------------------------------------------------------

#[test]
fn hir_container_accessors() {
    let tcx = TyCtxt::new();
    let i64t = tcx.int(crate::ty::IntTy::I64);
    let mut hir = Hir::new();

    let f = DefId(7);
    let p0 = LocalId(0);
    let mut locals = std::collections::HashMap::new();
    locals.insert(p0, i64t);

    hir.fn_sigs.insert(
        f,
        FnSig { params: vec![(p0, i64t)], ret: i64t, async_output: None },
    );
    hir.bodies.insert(
        f,
        Body {
            def: f,
            params: vec![p0],
            locals,
            ret: i64t,
            async_output: None,
            block: Block { stmts: vec![], trailing: None, ty: tcx.null, span: sp(0, 2) },
            span: sp(0, 2),
        },
    );

    assert!(hir.body(f).is_some());
    assert!(hir.body(DefId(99)).is_none());
    assert_eq!(hir.sig(f).unwrap().ret, i64t);
    assert_eq!(hir.body(f).unwrap().local_ty(p0), Some(i64t));
    assert_eq!(hir.body(f).unwrap().local_ty(LocalId(5)), None);
}

#[test]
fn program_level_tables_are_def_keyed() {
    let tcx = TyCtxt::new();
    let i32t = tcx.int(crate::ty::IntTy::I32);
    let mut hir = Hir::new();

    hir.extern_sigs.insert(DefId(1), ExternSig { params: vec![i32t], ret: tcx.null });
    hir.structs.insert(DefId(2), StructFields::Tuple(vec![i32t]));
    hir.iface_impls.insert((DefId(2), DefId(3)), DefId(4));
    hir.link_libs.push("z".into());
    hir.local_decls.insert(LocalId(0), sp(10, 14));

    assert_eq!(hir.extern_sigs[&DefId(1)].params, vec![i32t]);
    assert!(matches!(hir.structs[&DefId(2)], StructFields::Tuple(_)));
    assert_eq!(hir.iface_impls[&(DefId(2), DefId(3))], DefId(4));
    assert_eq!(hir.link_libs, vec!["z".to_string()]);
    assert_eq!(hir.local_decls[&LocalId(0)], sp(10, 14));
}

// ---------------------------------------------------------------------------
// Every expression node carries its type and span (provenance)
// ---------------------------------------------------------------------------

#[test]
fn literals_carry_type_and_span() {
    let tcx = TyCtxt::new();
    let i64t = tcx.int(crate::ty::IntTy::I64);
    let f64t = tcx.float(crate::ty::FloatTy::F64);

    let int = expr(ExprKind::Int(42), i64t, 0, 2);
    assert!(matches!(int.kind, ExprKind::Int(42)));
    assert_eq!(int.ty, i64t);
    assert_eq!(int.span, sp(0, 2));

    let fl = expr(ExprKind::Float(2.5), f64t, 3, 6);
    assert!(matches!(fl.kind, ExprKind::Float(v) if v == 2.5));

    let b = expr(ExprKind::Bool(true), tcx.bool, 7, 11);
    let n = expr(ExprKind::Null, tcx.null, 12, 16);
    let c = expr(ExprKind::Char('x' as u32), tcx.char, 17, 20);
    assert!(matches!(b.kind, ExprKind::Bool(true)));
    assert!(matches!(n.kind, ExprKind::Null));
    assert!(matches!(c.kind, ExprKind::Char(0x78)));
}

#[test]
fn string_interpolation_folds_stringify_method() {
    let tcx = TyCtxt::new();
    let parts = vec![
        StrPart::Text("n=".into()),
        StrPart::Interp {
            expr: Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), tcx.str, 4, 5)),
            stringify: Some(DefId(12)),
            stringify_targs: vec![],
        },
    ];
    let s = expr(ExprKind::Str(parts), tcx.str, 0, 6);
    let ExprKind::Str(ps) = &s.kind else { panic!() };
    assert_eq!(ps.len(), 2);
    match &ps[1] {
        StrPart::Interp { stringify, .. } => assert_eq!(*stringify, Some(DefId(12))),
        _ => panic!("expected interp hole"),
    }
}

#[test]
fn name_resolutions_cover_every_value_res() {
    let tcx = TyCtxt::new();
    let t = tcx.null;
    for res in [
        Res::Local(LocalId(3)),
        Res::Function(DefId(1)),
        Res::Method(DefId(2)),
        Res::Global(DefId(4)),
        Res::StructCtor(DefId(5)),
        Res::Builtin(Builtin::Println),
    ] {
        let e = expr(ExprKind::Name(res), t, 0, 1);
        assert!(matches!(e.kind, ExprKind::Name(_)));
    }
}

#[test]
fn call_kinds_capture_dispatch() {
    let tcx = TyCtxt::new();
    let t = tcx.null;
    let arg = || expr(ExprKind::Int(1), t, 0, 1);

    let cs = Span::dummy();
    let direct = ExprKind::Call {
        kind: CallKind::Direct { def: DefId(1), type_args: vec![t] },
        args: vec![arg()],
        callee_span: cs,
        callee_ty: t,
    };
    let method = ExprKind::Call {
        kind: CallKind::Method {
            def: DefId(2),
            type_args: vec![],
            recv_static: Some(t),
            is_static: true,
        },
        args: vec![],
        callee_span: cs,
        callee_ty: t,
    };
    let builtin = ExprKind::Call {
        kind: CallKind::Builtin(Builtin::Print),
        args: vec![arg()],
        callee_span: cs,
        callee_ty: t,
    };
    let closure = ExprKind::Call {
        kind: CallKind::Closure { callee: Box::new(arg()) },
        args: vec![],
        callee_span: cs,
        callee_ty: t,
    };
    let extern_c = ExprKind::Call {
        kind: CallKind::Extern { def: DefId(9) },
        args: vec![],
        callee_span: cs,
        callee_ty: t,
    };

    for k in [direct, method, builtin, closure, extern_c] {
        let _ = expr(k, t, 0, 4);
    }
}

#[test]
fn intrinsics_cover_every_retired_marker_table() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I32);
    let kinds = vec![
        Intrinsic::Num(NumIntrinsic::IntBound { ty: t, max: true }),
        Intrinsic::CollectionCtor,
        Intrinsic::Clone(CloneKind::Identity),
        Intrinsic::SharedNew,
        Intrinsic::ChannelNew,
        Intrinsic::ThreadSpawn { output: t, is_async: false },
        Intrinsic::ThreadJoin { output: t },
        Intrinsic::YieldNow,
        Intrinsic::AsyncSleep,
        Intrinsic::FutureCancel,
        Intrinsic::ForeignAlloc { ty: t, zeroed: false },
        Intrinsic::ForeignFree,
        Intrinsic::ForeignRealloc,
        Intrinsic::ForeignFlex { ty: t, elem: t },
        Intrinsic::CStringFromStr,
        Intrinsic::CStrToStr,
    ];
    assert_eq!(kinds.len(), 16);
    for intrinsic in kinds {
        let _ = expr(ExprKind::Intrinsic { intrinsic, args: vec![] }, t, 0, 3);
    }
}

#[test]
fn operators_carry_optional_overload() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let one = || Box::new(expr(ExprKind::Int(1), t, 0, 1));

    let prim = ExprKind::Binary { op: BinaryOp::Add, left: one(), right: one(), overload: None };
    let over = ExprKind::Binary {
        op: BinaryOp::Add,
        left: one(),
        right: one(),
        overload: Some(OpOverload { method: DefId(8), type_args: Vec::new() }),
    };
    let neg = ExprKind::Unary { op: UnaryOp::Neg, operand: one(), overload: None };
    if let ExprKind::Binary { overload: Some(ov), .. } = over {
        assert_eq!(ov.method, DefId(8));
    } else {
        panic!("expected an overloaded binary");
    }
    let _ = (prim, neg);
}

#[test]
fn cast_records_target_separately_from_result() {
    let tcx = TyCtxt::new();
    let i64t = tcx.int(crate::ty::IntTy::I64);
    // `e is T` — result type is bool, but the lowered target is the tested type.
    let c = expr(
        ExprKind::Cast {
            op: CastOp::Is,
            expr: Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), tcx.dynamic, 0, 1)),
            target: i64t,
        },
        tcx.bool,
        0,
        6,
    );
    assert_eq!(c.ty, tcx.bool);
    let ExprKind::Cast { target, op, .. } = &c.kind else { panic!() };
    assert_eq!(*target, i64t);
    assert_eq!(*op, CastOp::Is);
}

#[test]
fn adjust_is_an_explicit_wrapper_node() {
    let tcx = TyCtxt::new();
    let i64t = tcx.int(crate::ty::IntTy::I64);
    let dyn_t = tcx.dynamic;
    let inner = expr(ExprKind::Int(1), i64t, 0, 1);
    let widened = expr(
        ExprKind::Adjust { adjust: Adjust::Widen(dyn_t), expr: Box::new(inner) },
        dyn_t,
        0,
        1,
    );
    // The wrapper node's type is the post-coercion type; the inner keeps its own.
    assert_eq!(widened.ty, dyn_t);
    let ExprKind::Adjust { adjust, expr } = &widened.kind else { panic!() };
    assert!(matches!(adjust, Adjust::Widen(_)));
    assert_eq!(expr.ty, i64t);
}

#[test]
fn try_records_branch_and_residual_conversions() {
    let tcx = TyCtxt::new();
    let t = tcx.null;
    let inner = Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), t, 0, 1));
    let tnode = ExprKind::Try {
        expr: inner,
        branch: None,
        residual_conversions: vec![(t, DefId(3), t)],
    };
    if let ExprKind::Try { residual_conversions, .. } = &tnode {
        assert_eq!(residual_conversions.len(), 1);
    }
    let _ = expr(tnode, t, 0, 2);
}

#[test]
fn await_and_spawn_carry_output_types() {
    let tcx = TyCtxt::new();
    let i64t = tcx.int(crate::ty::IntTy::I64);
    let inner = || Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), tcx.dynamic, 0, 1));
    let aw = expr(ExprKind::Await { expr: inner(), output: i64t }, i64t, 0, 7);
    let sp_ = expr(ExprKind::Spawn { expr: inner(), output: i64t }, tcx.dynamic, 0, 8);
    if let ExprKind::Await { output, .. } = aw.kind {
        assert_eq!(output, i64t);
    }
    assert!(matches!(sp_.kind, ExprKind::Spawn { .. }));
}

#[test]
fn for_driver_covers_all_four_protocols() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let body = Block { stmts: vec![], trailing: None, ty: tcx.null, span: sp(8, 10) };
    let pat = Pattern { kind: PatternKind::Bind(LocalId(1)), ty: t, span: sp(4, 5) };
    let iter = Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), tcx.dynamic, 6, 7));

    let drivers = vec![
        ForDriver::ListFast { elem: t },
        ForDriver::Iter(ForIter {
            elem: t,
            next: DefId(1),
            next_targs: vec![],
            iter_ty: t,
            done_ty: t,
            item_ty: t,
        }),
        ForDriver::Map { key: t, value: t, entry: t },
        ForDriver::AsyncIter(ForAsyncIter {
            elem: t,
            next_async: DefId(2),
            next_targs: vec![],
            iter_ty: t,
            item_ty: t,
            done_ty: t,
            union_ty: t,
        }),
    ];
    assert_eq!(drivers.len(), 4);
    for driver in drivers {
        let _ = expr(
            ExprKind::For {
                pattern: pat.clone(),
                iter: iter.clone(),
                body: body.clone(),
                driver,
                in_async: false,
            },
            tcx.null,
            3,
            11,
        );
    }
}

#[test]
fn closure_and_async_block_carry_capture_analysis() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let body = Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), t, 5, 6));
    let clo = ExprKind::Closure {
        params: vec![(LocalId(0), t)],
        captures: vec![(LocalId(9), t)],
        ret: t,
        is_async: false,
        body,
    };
    if let ExprKind::Closure { captures, params, .. } = &clo {
        assert_eq!(captures.len(), 1);
        assert_eq!(params.len(), 1);
    }
    let ab = ExprKind::AsyncBlock {
        output: t,
        params: vec![],
        captures: vec![(LocalId(9), t)],
        body: Block { stmts: vec![], trailing: None, ty: t, span: sp(0, 2) },
    };
    let _ = (expr(clo, t, 0, 7), expr(ab, tcx.dynamic, 0, 9));
}

// ---------------------------------------------------------------------------
// Statements & blocks
// ---------------------------------------------------------------------------

#[test]
fn statements_cover_let_assign_expr_item() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let stmts = vec![
        Stmt {
            kind: StmtKind::Let {
                pattern: Pattern { kind: PatternKind::Bind(LocalId(0)), ty: t, span: sp(4, 5) },
                init: expr(ExprKind::Int(1), t, 8, 9),
            },
            span: sp(0, 10),
        },
        Stmt {
            kind: StmtKind::Assign {
                target: expr(ExprKind::Name(Res::Local(LocalId(0))), t, 11, 12),
                value: expr(ExprKind::Int(2), t, 15, 16),
            },
            span: sp(11, 17),
        },
        Stmt { kind: StmtKind::Expr(expr(ExprKind::Continue, tcx.never, 18, 26)), span: sp(18, 27) },
        Stmt { kind: StmtKind::Item(DefId(42)), span: sp(28, 40) },
    ];
    let block = Block { stmts, trailing: Some(Box::new(expr(ExprKind::Null, tcx.null, 41, 42))), ty: tcx.null, span: sp(0, 43) };
    assert_eq!(block.stmts.len(), 4);
    assert!(block.trailing.is_some());
    assert!(matches!(block.stmts[3].kind, StmtKind::Item(DefId(42))));
}

#[test]
fn assignment_discard_target() {
    let tcx = TyCtxt::new();
    let discard = expr(ExprKind::Discard, tcx.null, 0, 1);
    assert!(matches!(discard.kind, ExprKind::Discard));
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

#[test]
fn patterns_cover_every_kind_and_carry_type_tests() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let bind = |id| Pattern { kind: PatternKind::Bind(LocalId(id)), ty: t, span: sp(0, 1) };

    let kinds = vec![
        PatternKind::Wildcard,
        PatternKind::Bind(LocalId(0)),
        PatternKind::Literal(Box::new(expr(ExprKind::Int(1), t, 0, 1))),
        PatternKind::TypeBind { test_ty: t, bind: Some(LocalId(1)) },
        PatternKind::UnitPath { def: DefId(3), test_ty: t },
        PatternKind::TupleStruct { def: DefId(4), fields: vec![bind(2)], rest: None },
        PatternKind::RecordStruct {
            def: DefId(5),
            fields: vec![FieldPattern { index: 0, name: "x".into(), pattern: bind(3), span: sp(0, 1) }],
            has_rest: true,
        },
        PatternKind::Tuple { elems: vec![bind(4)], rest: Some((1, RestPattern { bind: Some(LocalId(5)), span: sp(0, 1) })) },
        PatternKind::List { elems: vec![bind(6)], rest: None },
        PatternKind::Or(vec![bind(7), bind(8)]),
    ];
    assert_eq!(kinds.len(), 10);

    // The type-test pattern keeps the tested variant separately from its node ty.
    if let PatternKind::TypeBind { test_ty, bind } = &kinds[3] {
        assert_eq!(*test_ty, t);
        assert_eq!(*bind, Some(LocalId(1)));
    }
    for kind in kinds {
        let _ = Pattern { kind, ty: t, span: sp(0, 4) };
    }
}

// ---------------------------------------------------------------------------
// Composite expressions
// ---------------------------------------------------------------------------

#[test]
fn composites_struct_tuple_list_map() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let one = || expr(ExprKind::Int(1), t, 0, 1);

    let tuple = expr(ExprKind::Tuple(vec![one(), one()]), t, 0, 5);
    let list = expr(ExprKind::List(vec![one()]), t, 0, 3);
    let map = expr(
        ExprKind::Map(vec![
            MapEntry::Kv { key: one(), value: one() },
            MapEntry::Spread(one()),
        ]),
        t,
        0,
        9,
    );
    let strukt = expr(
        ExprKind::Struct {
            def: DefId(1),
            type_args: vec![t],
            fields: vec![FieldInit { index: 0, name: "x".into(), value: one(), span: sp(2, 3) }],
            spread: Some(Box::new(one())),
        },
        t,
        0,
        12,
    );
    assert!(matches!(tuple.kind, ExprKind::Tuple(_)));
    assert!(matches!(list.kind, ExprKind::List(_)));
    if let ExprKind::Map(items) = &map.kind {
        assert_eq!(items.len(), 2);
    }
    if let ExprKind::Struct { fields, spread, .. } = &strukt.kind {
        assert_eq!(fields[0].name, "x");
        assert!(spread.is_some());
    }
}

#[test]
fn field_and_index_accesses() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let recv = || Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), t, 0, 1));
    let field = expr(
        ExprKind::Field {
            receiver: recv(),
            field: FieldRef { struct_def: DefId(1), index: 2, name: "z".into() },
        },
        t,
        0,
        3,
    );
    let tup_idx = expr(ExprKind::TupleIndex { receiver: recv(), index: 1 }, t, 0, 3);
    let index = expr(ExprKind::Index { receiver: recv(), index: recv() }, t, 0, 4);
    if let ExprKind::Field { field, .. } = &field.kind {
        assert_eq!(field.index, 2);
        assert_eq!(field.name, "z");
    }
    assert!(matches!(tup_idx.kind, ExprKind::TupleIndex { index: 1, .. }));
    assert!(matches!(index.kind, ExprKind::Index { .. }));
}

#[test]
fn control_flow_if_match_loops() {
    let tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let blk = || Block { stmts: vec![], trailing: None, ty: tcx.null, span: sp(0, 2) };
    let cond = || Box::new(expr(ExprKind::Bool(true), tcx.bool, 0, 4));

    let if_ = expr(
        ExprKind::If {
            cond: cond(),
            then_block: blk(),
            else_branch: Some(Box::new(expr(ExprKind::Block(blk()), tcx.null, 0, 2))),
        },
        tcx.null,
        0,
        10,
    );
    let match_ = expr(
        ExprKind::Match {
            scrutinee: Box::new(expr(ExprKind::Int(1), t, 0, 1)),
            arms: vec![MatchArm {
                pattern: Pattern { kind: PatternKind::Wildcard, ty: t, span: sp(0, 1) },
                guard: None,
                body: expr(ExprKind::Int(2), t, 0, 1),
                span: sp(0, 4),
            }],
        },
        t,
        0,
        12,
    );
    let while_ = expr(ExprKind::While { cond: cond(), body: blk() }, tcx.null, 0, 8);
    let loop_ = expr(ExprKind::Loop(blk()), tcx.never, 0, 6);
    let ret = expr(ExprKind::Return(Some(Box::new(expr(ExprKind::Int(0), t, 0, 1)))), tcx.never, 0, 8);
    let brk = expr(ExprKind::Break(None), tcx.never, 0, 5);

    assert!(matches!(if_.kind, ExprKind::If { .. }));
    if let ExprKind::Match { arms, .. } = &match_.kind {
        assert_eq!(arms.len(), 1);
    }
    let _ = (while_, loop_, ret, brk);
}

#[test]
fn ref_and_deref_ffi_nodes() {
    let mut tcx = TyCtxt::new();
    let t = tcx.int(crate::ty::IntTy::I64);
    let pt = tcx.mk_ptr(t);
    let inner = Box::new(expr(ExprKind::Name(Res::Local(LocalId(0))), t, 0, 1));
    let r = expr(ExprKind::Ref(inner.clone()), pt, 0, 2);
    let d = expr(ExprKind::Deref(inner), t, 0, 2);
    assert_eq!(r.ty, pt);
    assert!(matches!(d.kind, ExprKind::Deref(_)));
}
