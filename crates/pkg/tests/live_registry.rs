//! Live network round-trips against the built-in registry server (`docs/23`
//! §7). The offline `LocalRegistry` proves the resolver's logic; this proves the
//! HTTP *transport* — `HttpRegistry` talking to `pkg::server` over real TCP on
//! localhost: connect → publish → index → download → verify → search → yank.

use std::path::PathBuf;

use pkg::registry::{HttpRegistry, Registry};
use pkg::{server, store};

/// A unique temp dir for one test's registry state.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "otter_live_reg_{tag}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn full_publish_index_download_search_yank_round_trip() {
    let dir = temp_dir("rt");
    let token = "secret-token".to_string();
    let handle = server::serve(dir.clone(), Some(token.clone())).expect("start server");
    let base = handle.base_url();

    // Connect: fetches and parses `config.json` over HTTP.
    let reg = HttpRegistry::connect("local", &base, Some(token.clone()))
        .expect("connect to live registry");

    // A package with no versions yet has an empty index.
    assert!(reg.index("widget").expect("index widget").is_empty());

    // Publish two versions (the server computes each checksum from the bytes).
    let v1 = b"widget-1.0.0-tarball".to_vec();
    let v2 = b"widget-1.2.0-tarball".to_vec();
    reg.publish("widget", "1.0.0", &v1).expect("publish 1.0.0");
    reg.publish("widget", "1.2.0", &v2).expect("publish 1.2.0");

    // The index now lists both, newest discoverable.
    let entries = reg.index("widget").expect("index after publish");
    assert_eq!(entries.len(), 2, "expected two published versions");
    let e12 = entries
        .iter()
        .find(|e| e.vers.to_string() == "1.2.0")
        .expect("1.2.0 in index");
    assert!(!e12.yanked);

    // Download 1.2.0 and verify it against the index checksum.
    let bytes = reg.download(e12).expect("download 1.2.0");
    assert_eq!(bytes, v2, "downloaded bytes match what was published");
    store::verify(&bytes, &e12.cksum).expect("checksum verifies");

    // Search finds the package by substring, reporting its max version.
    let hits = reg.search("wid", 10).expect("search");
    let hit = hits
        .iter()
        .find(|h| h.name == "widget")
        .expect("widget in search");
    assert_eq!(hit.max_version, "1.2.0");

    // Yank 1.2.0; the index reflects it.
    reg.yank("widget", "1.2.0").expect("yank 1.2.0");
    let after = reg.index("widget").expect("index after yank");
    let e12 = after
        .iter()
        .find(|e| e.vers.to_string() == "1.2.0")
        .unwrap();
    assert!(e12.yanked, "1.2.0 should be yanked");

    // After yanking the max, search reports the remaining version.
    let hits = reg.search("widget", 10).expect("search after yank");
    let hit = hits
        .iter()
        .find(|h| h.name == "widget")
        .expect("widget still searchable");
    assert_eq!(hit.max_version, "1.0.0", "search ignores yanked versions");

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writes_require_the_token() {
    let dir = temp_dir("auth");
    let handle = server::serve(dir.clone(), Some("the-token".into())).expect("start server");
    let base = handle.base_url();

    // A client with the WRONG token can still read but not publish.
    let bad = HttpRegistry::connect("local", &base, Some("wrong".into())).expect("connect");
    assert!(bad.index("anything").expect("read is open").is_empty());
    assert!(
        bad.publish("anything", "1.0.0", b"x").is_err(),
        "publish with a bad token must fail"
    );

    // The right token succeeds.
    let good = HttpRegistry::connect("local", &base, Some("the-token".into())).expect("connect");
    good.publish("anything", "1.0.0", b"x")
        .expect("publish with valid token");
    assert_eq!(good.index("anything").expect("index").len(), 1);

    handle.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unknown_package_index_is_empty_not_an_error() {
    let dir = temp_dir("missing");
    let handle = server::serve(dir.clone(), None).expect("start server");
    let reg = HttpRegistry::connect("local", &handle.base_url(), None).expect("connect");
    // A 404 on the sparse index maps to an empty version list, never an error.
    assert!(reg.index("does-not-exist").expect("empty index").is_empty());
    handle.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
