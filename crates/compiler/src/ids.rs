//! Stable identifiers assigned during semantic analysis.
//!
//! Every named definition in the program — a struct, interface, function, type
//! alias, module, generic parameter, struct field, local binding — gets a
//! [`DefId`]. Ids are dense `u32`s handed out by the resolver, so they index
//! directly into side tables.

use std::fmt;

/// A globally unique id for a definition (item, field, generic param, local).
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DefId(pub u32);

impl DefId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for DefId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "def{}", self.0)
    }
}

/// A local binding (parameter or `var`), unique within a function body so that
/// shadowed names remain distinguishable for the checker and code generator.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct LocalId(pub u32);

impl LocalId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "local{}", self.0)
    }
}

/// A module in the module tree. The root (crate entry) is always `ModId(0)`.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ModId(pub u32);

impl ModId {
    pub const ROOT: ModId = ModId(0);

    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for ModId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mod{}", self.0)
    }
}
