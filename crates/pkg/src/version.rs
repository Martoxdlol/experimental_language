//! Semver resolution helpers (`docs/23` §7).
//!
//! The resolver picks **one version per (name, semver-compatible range)**:
//! requirements in the same compatibility range unify to the single highest
//! available version satisfying all of them (`^1.2` + `^1.4` → the newest
//! `1.x`), while requirements in *different* majors coexist as separate nodes.

pub use semver::{Version, VersionReq};

/// Parse a manifest version requirement. Bare versions are caret requirements,
/// matching cargo (`"1.2"` ≡ `"^1.2"`).
pub fn parse_req(s: &str) -> Result<VersionReq, String> {
    VersionReq::parse(s).map_err(|e| format!("invalid version requirement `{s}`: {e}"))
}

/// Parse an exact version.
pub fn parse_version(s: &str) -> Result<Version, String> {
    Version::parse(s).map_err(|e| format!("invalid version `{s}`: {e}"))
}

/// The highest `available` version satisfying *every* requirement in `reqs`
/// (yanked versions having been filtered by the caller), or `None` if none does.
pub fn pick_version(reqs: &[VersionReq], available: &[Version]) -> Option<Version> {
    available
        .iter()
        .filter(|v| reqs.iter().all(|r| r.matches(v)))
        .max()
        .cloned()
}

/// Group requirements by major compatibility range (`major`, or for `0.x` the
/// `0.minor` range, matching semver's caret semantics). Requirements that can
/// never share a version live in different groups, so different majors coexist.
pub fn compat_key(req: &VersionReq) -> Option<(u64, u64)> {
    // Use the first comparator's lower bound as the range key.
    let c = req.comparators.first()?;
    if c.major == 0 {
        Some((0, c.minor.unwrap_or(0)))
    } else {
        Some((c.major, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }
    fn r(s: &str) -> VersionReq {
        VersionReq::parse(s).unwrap()
    }

    #[test]
    fn bare_version_is_a_caret_requirement() {
        let req = parse_req("1.2").unwrap();
        assert!(req.matches(&v("1.4.0")));
        assert!(req.matches(&v("1.2.0")));
        assert!(!req.matches(&v("2.0.0")));
        assert!(!req.matches(&v("1.1.0")));
    }

    #[test]
    fn unifies_compatible_ranges_to_the_highest() {
        // ^1.2 + ^1.4 → newest 1.x satisfying both = 1.4.x.
        let reqs = [r("^1.2"), r("^1.4")];
        let available = [v("1.2.0"), v("1.4.0"), v("1.5.3"), v("2.0.0")];
        assert_eq!(pick_version(&reqs, &available), Some(v("1.5.3")));
    }

    #[test]
    fn incompatible_majors_have_no_shared_version() {
        let reqs = [r("^1"), r("^2")];
        let available = [v("1.5.0"), v("2.1.0")];
        assert_eq!(pick_version(&reqs, &available), None);
    }

    #[test]
    fn compat_key_separates_majors_and_zero_minors() {
        assert_eq!(compat_key(&r("^1.2")), Some((1, 0)));
        assert_eq!(compat_key(&r("^2.0")), Some((2, 0)));
        // 0.x: each minor is its own compatibility range.
        assert_eq!(compat_key(&r("^0.4")), Some((0, 4)));
        assert_eq!(compat_key(&r("^0.5")), Some((0, 5)));
    }

    #[test]
    fn pick_returns_none_when_nothing_matches() {
        assert_eq!(pick_version(&[r("^3")], &[v("1.0.0"), v("2.0.0")]), None);
    }
}
