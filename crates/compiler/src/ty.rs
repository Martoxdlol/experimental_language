//! Semantic types: the interned representation produced by lowering AST
//! `Type` nodes and inferred by the type checker.
//!
//! Types are **interned**: a [`Ty`] is a small copyable handle (a `u32`) into a
//! [`TyCtxt`]. Structural equality is therefore pointer (index) equality, which
//! is exactly what union set-semantics need — see [`TyCtxt::mk_union`].
//!
//! Discriminated unions are normalized on construction following
//! `docs/03-unions`: nested unions flatten, duplicates collapse, order is
//! irrelevant (a canonical sorted order is stored), and a single remaining
//! variant collapses to that variant. Alias expansion happens *before*
//! reaching this module, so structural identity holds across alias spellings.

use crate::ids::DefId;
use std::collections::HashMap;
use std::fmt;

/// Fixed-width and pointer-width integer types.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    Isize,
    Usize,
}

impl IntTy {
    pub fn is_signed(self) -> bool {
        use IntTy::*;
        matches!(self, I8 | I16 | I32 | I64 | Isize)
    }

    /// Bit width, or `None` for the platform-dependent `isize`/`usize`.
    pub fn bits(self) -> Option<u32> {
        use IntTy::*;
        Some(match self {
            I8 | U8 => 8,
            I16 | U16 => 16,
            I32 | U32 => 32,
            I64 | U64 => 64,
            Isize | Usize => return None,
        })
    }

    pub fn name(self) -> &'static str {
        use IntTy::*;
        match self {
            I8 => "i8",
            I16 => "i16",
            I32 => "i32",
            I64 => "i64",
            U8 => "u8",
            U16 => "u16",
            U32 => "u32",
            U64 => "u64",
            Isize => "isize",
            Usize => "usize",
        }
    }

    pub fn from_name(s: &str) -> Option<IntTy> {
        use IntTy::*;
        Some(match s {
            "i8" => I8,
            "i16" => I16,
            "i32" => I32,
            "i64" => I64,
            "u8" => U8,
            "u16" => U16,
            "u32" => U32,
            "u64" => U64,
            "isize" => Isize,
            "usize" => Usize,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug)]
pub enum FloatTy {
    F32,
    F64,
}

impl FloatTy {
    pub fn name(self) -> &'static str {
        match self {
            FloatTy::F32 => "f32",
            FloatTy::F64 => "f64",
        }
    }

    pub fn from_name(s: &str) -> Option<FloatTy> {
        match s {
            "f32" => Some(FloatTy::F32),
            "f64" => Some(FloatTy::F64),
            _ => None,
        }
    }
}

/// An interned type handle. Cheap to copy and compare.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Ty(u32);

impl fmt::Debug for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ty{}", self.0)
    }
}

/// An inference variable, resolved during type checking.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct InferVar(pub u32);

/// The structure of a type. Stored once per distinct type in the [`TyCtxt`].
#[derive(Clone, Eq, PartialEq, Hash, Debug)]
pub enum TyKind {
    Int(IntTy),
    Float(FloatTy),
    Bool,
    Char,
    /// The managed, immutable UTF-8 string type.
    Str,
    /// The empty/unit type and its sole value.
    Null,
    /// The universal type — union of every type, narrowed with `is`/`as`.
    Dynamic,
    /// The bottom type produced by diverging expressions; subtype of all.
    Never,
    /// A nominal type (struct / interface / opaque) applied to type arguments.
    /// Type aliases are expanded away before interning, so they never appear.
    Named { def: DefId, args: Vec<Ty> },
    /// A structural tuple. Never length 0 or 1.
    Tuple(Vec<Ty>),
    /// A function value type. `is_extern` marks the C-ABI `extern (..) => R`.
    Func {
        params: Vec<Ty>,
        ret: Ty,
        is_extern: bool,
    },
    /// A normalized discriminated union. Always length >= 2, stored sorted by
    /// the interned index of each variant for a canonical form.
    Union(Vec<Ty>),
    /// A raw FFI pointer `*T`.
    Ptr(Ty),
    /// A fixed-size FFI array `[T; N]`. `len` is recorded post-evaluation.
    Array { elem: Ty, len: u64 },
    /// `Self` inside an interface or `extend` body, before substitution.
    SelfTy,
    /// A generic parameter, identified by its definition.
    Param(DefId),
    /// An unresolved inference variable.
    Infer(InferVar),
    /// A placeholder produced after a type error, to keep checking going.
    Error,
}

/// Owns every interned type plus a cache of the primitives.
pub struct TyCtxt {
    kinds: Vec<TyKind>,
    intern: HashMap<TyKind, Ty>,

    // Cached primitives for fast access.
    pub bool: Ty,
    pub char: Ty,
    pub str: Ty,
    pub null: Ty,
    pub dynamic: Ty,
    pub never: Ty,
    pub error: Ty,
    ints: [Ty; 10],
    floats: [Ty; 2],
}

impl Default for TyCtxt {
    fn default() -> Self {
        Self::new()
    }
}

impl TyCtxt {
    pub fn new() -> Self {
        let mut cx = TyCtxt {
            kinds: Vec::new(),
            intern: HashMap::new(),
            bool: Ty(0),
            char: Ty(0),
            str: Ty(0),
            null: Ty(0),
            dynamic: Ty(0),
            never: Ty(0),
            error: Ty(0),
            ints: [Ty(0); 10],
            floats: [Ty(0); 2],
        };
        cx.bool = cx.intern(TyKind::Bool);
        cx.char = cx.intern(TyKind::Char);
        cx.str = cx.intern(TyKind::Str);
        cx.null = cx.intern(TyKind::Null);
        cx.dynamic = cx.intern(TyKind::Dynamic);
        cx.never = cx.intern(TyKind::Never);
        cx.error = cx.intern(TyKind::Error);
        use IntTy::*;
        for (i, it) in [I8, I16, I32, I64, U8, U16, U32, U64, Isize, Usize]
            .into_iter()
            .enumerate()
        {
            cx.ints[i] = cx.intern(TyKind::Int(it));
        }
        cx.floats[0] = cx.intern(TyKind::Float(FloatTy::F32));
        cx.floats[1] = cx.intern(TyKind::Float(FloatTy::F64));
        cx
    }

    /// Intern a kind, returning its stable handle.
    pub fn intern(&mut self, kind: TyKind) -> Ty {
        if let Some(&ty) = self.intern.get(&kind) {
            return ty;
        }
        let ty = Ty(self.kinds.len() as u32);
        self.kinds.push(kind.clone());
        self.intern.insert(kind, ty);
        ty
    }

    /// The structure behind a handle.
    #[inline]
    pub fn kind(&self, ty: Ty) -> &TyKind {
        &self.kinds[ty.0 as usize]
    }

    // -- primitive accessors -------------------------------------------------

    pub fn int(&self, it: IntTy) -> Ty {
        use IntTy::*;
        let idx = match it {
            I8 => 0,
            I16 => 1,
            I32 => 2,
            I64 => 3,
            U8 => 4,
            U16 => 5,
            U32 => 6,
            U64 => 7,
            Isize => 8,
            Usize => 9,
        };
        self.ints[idx]
    }

    pub fn float(&self, ft: FloatTy) -> Ty {
        self.floats[ft as usize]
    }

    pub fn mk_named(&mut self, def: DefId, args: Vec<Ty>) -> Ty {
        self.intern(TyKind::Named { def, args })
    }

    pub fn mk_ptr(&mut self, inner: Ty) -> Ty {
        self.intern(TyKind::Ptr(inner))
    }

    pub fn mk_param(&mut self, def: DefId) -> Ty {
        self.intern(TyKind::Param(def))
    }

    /// Build a tuple. A 0- or 1-element "tuple" is not a tuple type: 0 maps to
    /// `null` and 1 collapses to its element (parentheses are grouping).
    pub fn mk_tuple(&mut self, elems: Vec<Ty>) -> Ty {
        match elems.len() {
            0 => self.null,
            1 => elems[0],
            _ => self.intern(TyKind::Tuple(elems)),
        }
    }

    pub fn mk_func(&mut self, params: Vec<Ty>, ret: Ty, is_extern: bool) -> Ty {
        self.intern(TyKind::Func { params, ret, is_extern })
    }

    /// Construct a normalized union from a set of member types.
    ///
    /// Following `docs/03-unions` §2: nested unions flatten, duplicates
    /// collapse, order does not matter (a canonical order is stored). A single
    /// surviving variant collapses to that variant; an empty set yields
    /// [`TyCtxt::never`] (the identity for union — though the front end rejects
    /// writing an empty union, this keeps construction total).
    pub fn mk_union(&mut self, members: impl IntoIterator<Item = Ty>) -> Ty {
        let mut flat: Vec<Ty> = Vec::new();
        let mut stack: Vec<Ty> = members.into_iter().collect();
        // Flatten nested unions (and skip `never`, the absorbed bottom).
        while let Some(m) = stack.pop() {
            match self.kind(m) {
                TyKind::Union(inner) => stack.extend(inner.iter().copied()),
                TyKind::Never => {}
                _ => flat.push(m),
            }
        }
        // Canonicalize: sort by interned index, then dedup.
        flat.sort_unstable();
        flat.dedup();
        match flat.len() {
            0 => self.never,
            1 => flat[0],
            _ => self.intern(TyKind::Union(flat)),
        }
    }

    // -- queries -------------------------------------------------------------

    pub fn is_never(&self, ty: Ty) -> bool {
        matches!(self.kind(ty), TyKind::Never)
    }

    pub fn is_error(&self, ty: Ty) -> bool {
        matches!(self.kind(ty), TyKind::Error)
    }

    /// The set of variants of `ty` as a union: the singleton `{ty}` for a
    /// non-union, or its members for a union.
    pub fn variants(&self, ty: Ty) -> Vec<Ty> {
        match self.kind(ty) {
            TyKind::Union(v) => v.clone(),
            _ => vec![ty],
        }
    }

    /// Is every variant of `sub` also a variant of `sup`? This is union
    /// widening (`docs/03` §4): implicit subtyping by variant-set inclusion.
    /// `never` is a subtype of everything; identical types trivially hold.
    pub fn is_union_subtype(&self, sub: Ty, sup: Ty) -> bool {
        if sub == sup || self.is_never(sub) {
            return true;
        }
        let sup_vs = self.variants(sup);
        self.variants(sub)
            .iter()
            .all(|v| self.is_never(*v) || sup_vs.contains(v))
    }

    /// Render a type for diagnostics. `name_of` resolves a `DefId` to its
    /// source name (the resolver owns that table); pass a closure that looks it
    /// up. Used by the checker's error messages.
    pub fn display(&self, ty: Ty, name_of: &impl Fn(DefId) -> String) -> String {
        match self.kind(ty) {
            TyKind::Int(i) => i.name().to_string(),
            TyKind::Float(f) => f.name().to_string(),
            TyKind::Bool => "bool".into(),
            TyKind::Char => "char".into(),
            TyKind::Str => "str".into(),
            TyKind::Null => "null".into(),
            TyKind::Dynamic => "dynamic".into(),
            TyKind::Never => "never".into(),
            TyKind::Error => "<error>".into(),
            TyKind::SelfTy => "Self".into(),
            TyKind::Param(d) => name_of(*d),
            TyKind::Infer(v) => format!("?{}", v.0),
            TyKind::Ptr(inner) => format!("*{}", self.display(*inner, name_of)),
            TyKind::Array { elem, len } => {
                format!("[{}; {}]", self.display(*elem, name_of), len)
            }
            TyKind::Named { def, args } => {
                let base = name_of(*def);
                if args.is_empty() {
                    base
                } else {
                    let inner: Vec<_> =
                        args.iter().map(|a| self.display(*a, name_of)).collect();
                    format!("{}<{}>", base, inner.join(", "))
                }
            }
            TyKind::Tuple(elems) => {
                let inner: Vec<_> =
                    elems.iter().map(|e| self.display(*e, name_of)).collect();
                format!("({})", inner.join(", "))
            }
            TyKind::Func { params, ret, is_extern } => {
                let inner: Vec<_> =
                    params.iter().map(|p| self.display(*p, name_of)).collect();
                let prefix = if *is_extern { "extern " } else { "" };
                format!("{}({}) => {}", prefix, inner.join(", "), self.display(*ret, name_of))
            }
            TyKind::Union(members) => {
                let inner: Vec<_> =
                    members.iter().map(|m| self.display(*m, name_of)).collect();
                inner.join(" | ")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_names(_: DefId) -> String {
        "T".into()
    }

    #[test]
    fn primitives_are_interned_once() {
        let cx = TyCtxt::new();
        assert_eq!(cx.int(IntTy::I64), cx.int(IntTy::I64));
        assert_ne!(cx.int(IntTy::I64), cx.int(IntTy::I32));
        assert_ne!(cx.bool, cx.char);
    }

    #[test]
    fn union_is_order_independent() {
        let mut cx = TyCtxt::new();
        let a = cx.mk_union([cx.int(IntTy::I64), cx.str]);
        let b = cx.mk_union([cx.str, cx.int(IntTy::I64)]);
        assert_eq!(a, b);
    }

    #[test]
    fn union_dedups() {
        let mut cx = TyCtxt::new();
        let i64 = cx.int(IntTy::I64);
        let u = cx.mk_union([i64, i64]);
        assert_eq!(u, i64, "duplicate variants collapse to the underlying type");
    }

    #[test]
    fn union_flattens_nested() {
        let mut cx = TyCtxt::new();
        let i64 = cx.int(IntTy::I64);
        let inner = cx.mk_union([i64, cx.str]);
        let outer = cx.mk_union([inner, cx.bool]);
        let direct = cx.mk_union([i64, cx.str, cx.bool]);
        assert_eq!(outer, direct);
        match cx.kind(outer) {
            TyKind::Union(v) => assert_eq!(v.len(), 3),
            _ => panic!("expected union"),
        }
    }

    #[test]
    fn never_is_absorbed_in_unions() {
        let mut cx = TyCtxt::new();
        let i64 = cx.int(IntTy::I64);
        let u = cx.mk_union([i64, cx.never]);
        assert_eq!(u, i64);
    }

    #[test]
    fn single_element_tuple_collapses() {
        let mut cx = TyCtxt::new();
        let i64 = cx.int(IntTy::I64);
        assert_eq!(cx.mk_tuple(vec![i64]), i64);
        assert_eq!(cx.mk_tuple(vec![]), cx.null);
    }

    #[test]
    fn union_subtype_is_variant_inclusion() {
        let mut cx = TyCtxt::new();
        let i64 = cx.int(IntTy::I64);
        let ab = cx.mk_union([i64, cx.str]);
        let abc = cx.mk_union([i64, cx.str, cx.bool]);
        assert!(cx.is_union_subtype(ab, abc));
        assert!(!cx.is_union_subtype(abc, ab));
        assert!(cx.is_union_subtype(i64, ab));
        assert!(cx.is_never(cx.never));
        assert!(cx.is_union_subtype(cx.never, abc));
    }

    #[test]
    fn display_renders_unions_and_generics() {
        let mut cx = TyCtxt::new();
        let i64 = cx.int(IntTy::I64);
        let u = cx.mk_union([i64, cx.null]);
        // Variant order is canonical (by intern index), not source order.
        assert_eq!(cx.display(u, &no_names), "null | i64");
        let named = cx.mk_named(DefId(0), vec![i64]);
        assert_eq!(cx.display(named, &no_names), "T<i64>");
    }
}
