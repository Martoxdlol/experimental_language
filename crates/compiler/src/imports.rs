//! Import-path classification (`docs/17` §17.4).
//!
//! Every import path carries an explicit **scheme prefix** — there is no
//! prefix-less form. This module is the single, mode-independent parser that
//! turns the raw string inside `import { … } from "<path>"` into a structured
//! [`ImportPath`]. It answers *what kind of path is this and what does it name*;
//! it does **not** decide *whether the path is available* (that depends on the
//! run mode — see the resolver in `sema`) nor does it touch the filesystem.
//!
//! The five live scheme families:
//!
//! | Scheme        | Form                              | Meaning                                   |
//! |---------------|-----------------------------------|-------------------------------------------|
//! | [`Scheme::Core`] | `core:collections`             | toolchain module, allocator-only          |
//! | [`Scheme::Std`]  | `std:io`                       | toolchain module, OS-backed               |
//! | [`Scheme::Pkg`]  | `pkg:json`, `pkg:json/parse`   | external dependency (first segment = name)|
//! | [`Scheme::SelfRoot`] | `self:util/log`            | this package, from the source root        |
//! | [`Scheme::SelfRel`]  | `self:./sib`, `self:../up` | this package, relative to the importer    |
//! | [`Scheme::File`]     | `file:./data`, `file:../x` | raw filesystem path, relative to importer |
//!
//! Reserved/deferred scheme families (`docs/17` §17.14) — `url:`, `http:`,
//! `https:`, the `pkg+https://…` form, and `*+blob:` — are recognized and
//! rejected with a pointed error rather than mis-parsed.

use std::fmt;

/// The scheme family of a classified [`ImportPath`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// `core:…` — toolchain module assuming only an allocator (no OS).
    Core,
    /// `std:…` — toolchain module assuming an OS.
    Std,
    /// `pkg:<name>[/<sub>…]` — an external dependency. The first segment is the
    /// package name; the rest address into that package's public module tree.
    Pkg,
    /// `self:<segment>/…` — a module in the current package, addressed from the
    /// package (source) root.
    SelfRoot,
    /// `self:./…` / `self:../…` — a module in the current package, addressed
    /// relative to the importing file's directory.
    SelfRel,
    /// `file:./…` / `file:../…` — a raw filesystem path relative to the
    /// importing file. Not a module-tree lookup; may escape the package (gated).
    File,
}

impl Scheme {
    /// The textual scheme keyword (without the trailing colon).
    pub fn keyword(self) -> &'static str {
        match self {
            Scheme::Core => "core",
            Scheme::Std => "std",
            Scheme::Pkg => "pkg",
            Scheme::SelfRoot | Scheme::SelfRel => "self",
            Scheme::File => "file",
        }
    }

    /// Whether this scheme addresses the module tree (vs. a raw `file:` path).
    pub fn is_module_tree(self) -> bool {
        !matches!(self, Scheme::File)
    }

    /// Whether resolving this scheme requires **project context** (a manifest +
    /// module tree). `pkg:` and *both* `self:` forms do — without a project
    /// there is no module tree to anchor to. `core:`/`std:` (toolchain) and
    /// `file:` (a raw path) work in direct mode too (`docs/17` §17.13).
    pub fn requires_project_context(self) -> bool {
        matches!(self, Scheme::Pkg | Scheme::SelfRoot | Scheme::SelfRel)
    }
}

/// A classified import path (`docs/17` §17.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPath {
    /// The scheme family.
    pub scheme: Scheme,
    /// Path segments after the scheme, with any leading `./`/`../` markers
    /// removed (their effect is captured by [`Self::up`]). For [`Scheme::File`]
    /// the segments are the literal path components; otherwise they are module
    /// names. Never contains `.` or `..`.
    pub segments: Vec<String>,
    /// For the relative forms ([`Scheme::SelfRel`], [`Scheme::File`]), the
    /// number of leading `../` parent hops. A `./`-rooted path has `up == 0`.
    /// Always `0` for the absolute forms (`core:`/`std:`/`pkg:`/`self:` root).
    pub up: usize,
    /// The raw body after `scheme:`, verbatim — used for diagnostics and for
    /// the literal `file:` filesystem path.
    pub body: String,
}

impl ImportPath {
    /// The original source spelling (`scheme:body`), for diagnostics.
    pub fn display_source(&self) -> String {
        format!("{}:{}", self.scheme.keyword(), self.body)
    }

    /// For a [`Scheme::Pkg`] path, the dependency (package) name — its first
    /// segment. `None` if there are no segments (a malformed `pkg:` would have
    /// failed [`classify`] first, so live `Pkg` paths always have one).
    pub fn package_name(&self) -> Option<&str> {
        if self.scheme == Scheme::Pkg {
            self.segments.first().map(String::as_str)
        } else {
            None
        }
    }

    /// The segments addressing *into* a `pkg:` dependency, after its name.
    /// `pkg:json` → `[]`; `pkg:json/a/b` → `["a", "b"]`.
    pub fn package_subpath(&self) -> &[String] {
        if self.scheme == Scheme::Pkg && !self.segments.is_empty() {
            &self.segments[1..]
        } else {
            &[]
        }
    }
}

/// Why a path string is not a valid import path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportPathError {
    /// No `scheme:` prefix at all (`docs/17` §17.4: there is no prefix-less form).
    MissingScheme { path: String },
    /// A recognized-but-not-implemented scheme family (`docs/17` §17.14):
    /// `url:`, `http:`, `https:`, or a `<scheme>+<transport>` form like
    /// `pkg+https`.
    ReservedScheme { scheme: String },
    /// A scheme keyword that is not one of the five live families.
    UnknownScheme { scheme: String },
    /// The body after the scheme was empty (e.g. `core:` or `self:`).
    EmptyPath { scheme: String },
    /// A `file:` path that is not relative (must start with `./` or `../`).
    FileNotRelative { body: String },
    /// A `self:` / `file:` relative path with a `..` component after a real
    /// segment (parent hops must lead the path).
    InteriorParent { path: String },
}

impl fmt::Display for ImportPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportPathError::MissingScheme { path } => write!(
                f,
                "import path `{path}` has no scheme prefix; every import must name its origin \
                 (`core:`, `std:`, `pkg:`, `self:`, or `file:`)"
            ),
            ImportPathError::ReservedScheme { scheme } => write!(
                f,
                "`{scheme}:` imports are reserved but not implemented; a registry package is \
                 imported as `pkg:<name>` (its location lives in the manifest), and there is no \
                 `pkg+https://…` form (`docs/17` §17.14)"
            ),
            ImportPathError::UnknownScheme { scheme } => write!(
                f,
                "unknown import scheme `{scheme}:`; expected one of `core:`, `std:`, `pkg:`, \
                 `self:`, or `file:`"
            ),
            ImportPathError::EmptyPath { scheme } => {
                write!(f, "import path `{scheme}:` is empty")
            }
            ImportPathError::FileNotRelative { body } => write!(
                f,
                "`file:` path `{body}` must be relative to the importing file (start with `./` \
                 or `../`)"
            ),
            ImportPathError::InteriorParent { path } => write!(
                f,
                "import path `{path}` has a `..` after a path segment; parent (`..`) hops must \
                 lead the path"
            ),
        }
    }
}

/// Classify a raw import-path string into a structured [`ImportPath`], or report
/// why it is malformed. Mode-independent: this never decides availability.
pub fn classify(raw: &str) -> Result<ImportPath, ImportPathError> {
    let Some((scheme_kw, body)) = raw.split_once(':') else {
        return Err(ImportPathError::MissingScheme {
            path: raw.to_string(),
        });
    };

    // Reserved scheme families (`docs/17` §17.14). A `<scheme>+<transport>`
    // form (e.g. `pkg+https`) and the URL family are recognized and rejected.
    if scheme_kw.contains('+') {
        return Err(ImportPathError::ReservedScheme {
            scheme: scheme_kw.to_string(),
        });
    }
    match scheme_kw {
        "url" | "http" | "https" | "blob" => {
            return Err(ImportPathError::ReservedScheme {
                scheme: scheme_kw.to_string(),
            });
        }
        _ => {}
    }

    let scheme_for_empty = scheme_kw.to_string();
    if body.is_empty() {
        return Err(ImportPathError::EmptyPath {
            scheme: scheme_for_empty,
        });
    }

    match scheme_kw {
        "core" => Ok(absolute(Scheme::Core, body)?),
        "std" => Ok(absolute(Scheme::Std, body)?),
        "pkg" => Ok(absolute(Scheme::Pkg, body)?),
        "self" => classify_self(body),
        "file" => classify_file(body),
        other => Err(ImportPathError::UnknownScheme {
            scheme: other.to_string(),
        }),
    }
}

/// An absolute module path (`core:`/`std:`/`pkg:` / `self:` root): split on `/`,
/// reject empty/`.`/`..` components.
fn absolute(scheme: Scheme, body: &str) -> Result<ImportPath, ImportPathError> {
    let mut segments = Vec::new();
    for seg in body.split('/') {
        if seg.is_empty() {
            continue;
        }
        if seg == "." || seg == ".." {
            return Err(ImportPathError::InteriorParent {
                path: format!("{}:{}", scheme.keyword(), body),
            });
        }
        segments.push(seg.to_string());
    }
    if segments.is_empty() {
        return Err(ImportPathError::EmptyPath {
            scheme: scheme.keyword().to_string(),
        });
    }
    Ok(ImportPath {
        scheme,
        segments,
        up: 0,
        body: body.to_string(),
    })
}

/// `self:…` — either the root form (`self:util/log`) or the relative form
/// (`self:./sib`, `self:../up`), distinguished by a leading `.`/`..`.
fn classify_self(body: &str) -> Result<ImportPath, ImportPathError> {
    let parts: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
    let leads_relative = matches!(parts.first(), Some(&".") | Some(&".."));
    if !leads_relative {
        // Root form: trace the mod tree from the package root.
        return absolute(Scheme::SelfRoot, body);
    }
    let (up, segments) = split_relative(body, &parts)?;
    Ok(ImportPath {
        scheme: Scheme::SelfRel,
        segments,
        up,
        body: body.to_string(),
    })
}

/// `file:…` — a literal filesystem path, required to be relative (`./`/`../`).
fn classify_file(body: &str) -> Result<ImportPath, ImportPathError> {
    let parts: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
    if !matches!(parts.first(), Some(&".") | Some(&"..")) {
        return Err(ImportPathError::FileNotRelative {
            body: body.to_string(),
        });
    }
    let (up, segments) = split_relative(body, &parts)?;
    Ok(ImportPath {
        scheme: Scheme::File,
        segments,
        up,
        body: body.to_string(),
    })
}

/// Split a relative path's `/`-parts into a leading `../` hop count and the real
/// segments. `.` contributes nothing; `..` must lead (no interior parents).
fn split_relative(body: &str, parts: &[&str]) -> Result<(usize, Vec<String>), ImportPathError> {
    let mut up = 0usize;
    let mut segments = Vec::new();
    let mut seen_real = false;
    for &p in parts {
        match p {
            "." => {
                if seen_real {
                    // A `.` after a real segment is harmless; keep it a no-op.
                    continue;
                }
            }
            ".." => {
                if seen_real {
                    return Err(ImportPathError::InteriorParent {
                        path: format!("file/self:{body}"),
                    });
                }
                up += 1;
            }
            seg => {
                seen_real = true;
                segments.push(seg.to_string());
            }
        }
    }
    Ok((up, segments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> ImportPath {
        classify(raw).unwrap_or_else(|e| panic!("`{raw}` should classify, got {e:?}"))
    }

    #[test]
    fn core_and_std_are_absolute_module_paths() {
        let p = ok("core:collections");
        assert_eq!(p.scheme, Scheme::Core);
        assert_eq!(p.segments, ["collections"]);
        assert_eq!(p.up, 0);

        let p = ok("std:io");
        assert_eq!(p.scheme, Scheme::Std);
        assert_eq!(p.segments, ["io"]);

        // core: may have subpaths.
        let p = ok("core:prelude");
        assert_eq!(p.segments, ["prelude"]);
    }

    #[test]
    fn pkg_first_segment_is_the_package_name() {
        let p = ok("pkg:json");
        assert_eq!(p.scheme, Scheme::Pkg);
        assert_eq!(p.package_name(), Some("json"));
        assert!(p.package_subpath().is_empty());

        let p = ok("pkg:json/parse/inner");
        assert_eq!(p.package_name(), Some("json"));
        assert_eq!(p.package_subpath(), ["parse", "inner"]);
    }

    #[test]
    fn self_root_traces_from_package_root() {
        let p = ok("self:util/log");
        assert_eq!(p.scheme, Scheme::SelfRoot);
        assert_eq!(p.segments, ["util", "log"]);
        assert_eq!(p.up, 0);
        assert!(p.scheme.requires_project_context());
    }

    #[test]
    fn self_relative_counts_parent_hops() {
        let p = ok("self:./helper");
        assert_eq!(p.scheme, Scheme::SelfRel);
        assert_eq!(p.up, 0);
        assert_eq!(p.segments, ["helper"]);
        // Both `self:` forms need a project (no module tree without one).
        assert!(p.scheme.requires_project_context());

        let p = ok("self:../shared/util");
        assert_eq!(p.up, 1);
        assert_eq!(p.segments, ["shared", "util"]);

        let p = ok("self:../../util/log");
        assert_eq!(p.up, 2);
        assert_eq!(p.segments, ["util", "log"]);
    }

    #[test]
    fn file_must_be_relative() {
        let p = ok("file:./fixtures/data");
        assert_eq!(p.scheme, Scheme::File);
        assert_eq!(p.up, 0);
        assert_eq!(p.segments, ["fixtures", "data"]);
        assert!(!p.scheme.is_module_tree());

        let p = ok("file:../assets/x");
        assert_eq!(p.up, 1);
        assert_eq!(p.segments, ["assets", "x"]);

        assert_eq!(
            classify("file:data.csv"),
            Err(ImportPathError::FileNotRelative {
                body: "data.csv".into()
            })
        );
    }

    #[test]
    fn prefixless_paths_are_rejected() {
        assert_eq!(
            classify("geometry/vec"),
            Err(ImportPathError::MissingScheme {
                path: "geometry/vec".into()
            })
        );
        assert_eq!(
            classify("util"),
            Err(ImportPathError::MissingScheme {
                path: "util".into()
            })
        );
    }

    #[test]
    fn reserved_schemes_are_pointed_errors() {
        for s in ["url", "http", "https", "blob"] {
            assert_eq!(
                classify(&format!("{s}://example.com/x")),
                Err(ImportPathError::ReservedScheme { scheme: s.into() })
            );
        }
        // No `pkg+https://…` form.
        assert_eq!(
            classify("pkg+https://x/y"),
            Err(ImportPathError::ReservedScheme {
                scheme: "pkg+https".into()
            })
        );
    }

    #[test]
    fn unknown_scheme_lists_the_valid_ones() {
        assert_eq!(
            classify("bogus:thing"),
            Err(ImportPathError::UnknownScheme {
                scheme: "bogus".into()
            })
        );
    }

    #[test]
    fn empty_bodies_are_rejected() {
        assert_eq!(
            classify("core:"),
            Err(ImportPathError::EmptyPath {
                scheme: "core".into()
            })
        );
        assert_eq!(
            classify("self:"),
            Err(ImportPathError::EmptyPath {
                scheme: "self".into()
            })
        );
    }

    #[test]
    fn interior_parent_is_rejected() {
        assert!(matches!(
            classify("self:../foo/../bar"),
            Err(ImportPathError::InteriorParent { .. })
        ));
        assert!(matches!(
            classify("self:util/../log"),
            Err(ImportPathError::InteriorParent { .. })
        ));
    }

    #[test]
    fn display_source_round_trips_the_spelling() {
        assert_eq!(ok("self:util/log").display_source(), "self:util/log");
        assert_eq!(ok("pkg:json/parse").display_source(), "pkg:json/parse");
        assert_eq!(ok("file:./a/b").display_source(), "file:./a/b");
    }
}
