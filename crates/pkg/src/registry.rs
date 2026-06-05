//! The registry protocol (`docs/23` §7).
//!
//! A registry is a named index the resolver maps `pkg:` dependencies to. The
//! wire protocol is the **sparse HTTP** index: the client fetches only the
//! per-package metadata it needs. The index root serves a `config.json`:
//!
//! ```json
//! { "dl": "https://…/{crate}/{version}/{sha256-checksum}.tar.gz",
//!   "api": "https://…", "auth-required": false }
//! ```
//!
//! Per-package metadata is JSON-lines, one line per published version. A package
//! is a `.tar.gz` resolved through `dl`, verified by its `sha256`, and extracted
//! into the content-addressed store.
//!
//! [`Registry`] abstracts the transport so the resolver is testable: the real
//! [`HttpRegistry`] speaks sparse HTTP, while [`LocalRegistry`] serves a fixture
//! directory (used throughout the tests — no live network).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::version::{Version, VersionReq};

/// The registry `config.json`.
#[derive(Clone, Debug, Deserialize)]
pub struct RegistryConfig {
    /// Tarball download URL template.
    pub dl: String,
    /// Base URL for write/query operations (publish/yank/search/login). Optional.
    #[serde(default)]
    pub api: Option<String>,
    /// Private registry — every request carries credentials.
    #[serde(default, rename = "auth-required")]
    pub auth_required: bool,
}

/// One published version's index metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub vers: Version,
    pub deps: Vec<IndexDep>,
    /// Feature name -> feature/dependency entries enabled by that feature.
    pub features: BTreeMap<String, Vec<String>>,
    /// The tarball `sha256` (hex, no prefix).
    pub cksum: String,
    pub yanked: bool,
}

/// A dependency edge recorded in the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDep {
    pub name: String,
    pub req: VersionReq,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<String>,
    /// A non-default registry this dep resolves against, if any.
    pub registry: Option<String>,
}

/// Metadata sent alongside a published tarball so the registry can write a
/// complete sparse-index line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishMetadata {
    pub name: String,
    pub vers: String,
    pub deps: Vec<IndexDep>,
    pub features: BTreeMap<String, Vec<String>>,
}

/// Errors from registry operations.
#[derive(Debug)]
pub enum RegistryError {
    /// Network/IO failure.
    Transport(String),
    /// Malformed index/config payload.
    Protocol(String),
    /// The package is not in the index.
    NotFound(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Transport(m) => write!(f, "registry transport error: {m}"),
            RegistryError::Protocol(m) => write!(f, "registry protocol error: {m}"),
            RegistryError::NotFound(m) => write!(f, "package `{m}` not found in the registry"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// A registry the resolver can query. Implementations encapsulate the index
/// transport, `config.json`, and tarball download.
pub trait Registry {
    /// The registry's name (as declared in `[registries]`).
    fn name(&self) -> &str;
    /// Every published version of `pkg`, parsed from its sparse-index file.
    /// Returns an empty vec for an unknown package.
    fn index(&self, pkg: &str) -> Result<Vec<IndexEntry>, RegistryError>;
    /// Download the tarball bytes for a resolved index entry (verification is
    /// the caller's, via [`crate::store::verify`]).
    fn download(&self, entry: &IndexEntry) -> Result<Vec<u8>, RegistryError>;
}

/// Parse a sparse-index JSON-lines payload into entries (blank lines skipped).
pub fn parse_index(text: &str) -> Result<Vec<IndexEntry>, RegistryError> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw: RawIndexLine = serde_json::from_str(line)
            .map_err(|e| RegistryError::Protocol(format!("bad index line: {e}")))?;
        out.push(raw.into_entry()?);
    }
    Ok(out)
}

/// Substitute a `dl` template's markers (`{crate}`, `{version}`, `{prefix}`,
/// `{sha256-checksum}`). If none are present, `/{crate}/{version}/download` is
/// appended (`docs/23` §7).
pub fn dl_url(template: &str, name: &str, version: &str, cksum: &str) -> String {
    let markers = ["{crate}", "{version}", "{prefix}", "{sha256-checksum}"];
    if markers.iter().any(|m| template.contains(m)) {
        let prefix = index_prefix(name);
        template
            .replace("{crate}", name)
            .replace("{version}", version)
            .replace("{prefix}", &prefix)
            .replace("{sha256-checksum}", cksum)
    } else {
        format!(
            "{}/{name}/{version}/download",
            template.trim_end_matches('/')
        )
    }
}

/// The sparse-index prefix segment for `name` (`se/rd` for `serde`), used by the
/// `{prefix}` download marker.
fn index_prefix(name: &str) -> String {
    let l = name.to_lowercase();
    match l.len() {
        1 => format!("1"),
        2 => format!("2"),
        3 => format!("3/{}", &l[0..1]),
        _ => format!("{}/{}", &l[0..2], &l[2..4]),
    }
}

// --- HTTP transport ---------------------------------------------------------

/// A registry served over sparse HTTP (`sparse+https://…`).
pub struct HttpRegistry {
    name: String,
    /// Index base URL (the `sparse+` prefix stripped).
    base: String,
    config: RegistryConfig,
    /// Bearer token for a private (`auth-required`) registry.
    token: Option<String>,
}

impl HttpRegistry {
    /// Connect to a registry, fetching its `config.json`. `index_url` may carry
    /// the `sparse+` scheme prefix from the manifest.
    pub fn connect(
        name: &str,
        index_url: &str,
        token: Option<String>,
    ) -> Result<HttpRegistry, RegistryError> {
        let base = index_url
            .strip_prefix("sparse+")
            .unwrap_or(index_url)
            .trim_end_matches('/')
            .to_string();
        let cfg_text = http_get(&format!("{base}/config.json"), token.as_deref())?;
        let config: RegistryConfig = serde_json::from_str(&cfg_text)
            .map_err(|e| RegistryError::Protocol(format!("bad config.json: {e}")))?;
        Ok(HttpRegistry {
            name: name.to_string(),
            base,
            config,
            token,
        })
    }
}

impl Registry for HttpRegistry {
    fn name(&self) -> &str {
        &self.name
    }

    fn index(&self, pkg: &str) -> Result<Vec<IndexEntry>, RegistryError> {
        let prefix = index_prefix(pkg);
        let url = format!("{}/{prefix}/{}", self.base, pkg.to_lowercase());
        match http_get(&url, self.token.as_deref()) {
            Ok(text) => parse_index(&text),
            Err(RegistryError::NotFound(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn download(&self, entry: &IndexEntry) -> Result<Vec<u8>, RegistryError> {
        let url = dl_url(
            &self.config.dl,
            &entry.name,
            &entry.vers.to_string(),
            &entry.cksum,
        );
        http_get_bytes(&url, self.token.as_deref())
    }
}

/// One row of a `search` result.
#[derive(Clone, Debug, Deserialize)]
pub struct SearchHit {
    pub name: String,
    #[serde(default)]
    pub max_version: String,
    #[serde(default)]
    pub description: String,
}

impl HttpRegistry {
    /// The `config.json` for this registry.
    pub fn config(&self) -> &RegistryConfig {
        &self.config
    }

    /// The API base URL (`config.json` `api`), required for write/query ops.
    fn api(&self) -> Result<&str, RegistryError> {
        self.config.api.as_deref().ok_or_else(|| {
            RegistryError::Protocol(
                "this registry has no `api` URL (publish/yank/search unavailable)".into(),
            )
        })
    }

    /// Search the registry (`GET <api>/api/v1/crates?q=…`).
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, RegistryError> {
        let url = format!(
            "{}/api/v1/crates?q={}&per_page={limit}",
            self.api()?,
            urlencode(query)
        );
        let text = http_get(&url, self.token.as_deref())?;
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            crates: Vec<SearchHit>,
        }
        let resp: Resp = serde_json::from_str(&text)
            .map_err(|e| RegistryError::Protocol(format!("bad search response: {e}")))?;
        Ok(resp.crates)
    }

    /// Publish a packaged tarball and its sparse-index metadata sidecar.
    /// Requires a token.
    pub fn publish(
        &self,
        name: &str,
        version: &str,
        tarball: &[u8],
        metadata: &PublishMetadata,
    ) -> Result<(), RegistryError> {
        let url = format!("{}/api/v1/crates/{name}/{version}/publish", self.api()?);
        let body = encode_publish_body(metadata, tarball)?;
        self.put_authed(&url, &body)
    }

    /// Yank a published version (`DELETE <api>/api/v1/crates/<name>/<version>/yank`).
    pub fn yank(&self, name: &str, version: &str) -> Result<(), RegistryError> {
        let url = format!("{}/api/v1/crates/{name}/{version}/yank", self.api()?);
        let token = self.token.as_deref().ok_or_else(|| {
            RegistryError::Protocol("not logged in (run `otter_fusion login`)".into())
        })?;
        ureq::delete(&url)
            .set("Authorization", &format!("Bearer {token}"))
            .call()
            .map(|_| ())
            .map_err(|e| RegistryError::Transport(e.to_string()))
    }

    fn put_authed(&self, url: &str, body: &[u8]) -> Result<(), RegistryError> {
        let token = self.token.as_deref().ok_or_else(|| {
            RegistryError::Protocol("not logged in (run `otter_fusion login`)".into())
        })?;
        ureq::put(url)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/octet-stream")
            .send_bytes(body)
            .map(|_| ())
            .map_err(|e| RegistryError::Transport(e.to_string()))
    }
}

/// Minimal percent-encoding for a query string value.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn http_get(url: &str, token: Option<&str>) -> Result<String, RegistryError> {
    let bytes = http_get_bytes(url, token)?;
    String::from_utf8(bytes).map_err(|e| RegistryError::Protocol(format!("non-UTF-8 body: {e}")))
}

fn http_get_bytes(url: &str, token: Option<&str>) -> Result<Vec<u8>, RegistryError> {
    let mut req = ureq::get(url);
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    match req.call() {
        Ok(resp) => {
            let mut buf = Vec::new();
            resp.into_reader()
                .read_to_end(&mut buf)
                .map_err(|e| RegistryError::Transport(e.to_string()))?;
            Ok(buf)
        }
        Err(ureq::Error::Status(404, _)) => Err(RegistryError::NotFound(url.to_string())),
        Err(e) => Err(RegistryError::Transport(e.to_string())),
    }
}

use std::io::Read;

// --- Publish payload --------------------------------------------------------

const PUBLISH_PAYLOAD_MAGIC: &[u8] = b"otter-fusion-publish-v1\n";

/// Encode a publish request body as:
///
/// `MAGIC`, decimal metadata byte length, `\n`, UTF-8 JSON metadata, tarball.
///
/// The explicit length keeps arbitrary tarball bytes out of the JSON parser and
/// lets the registry reject truncated sidecars cleanly.
pub fn encode_publish_body(
    metadata: &PublishMetadata,
    tarball: &[u8],
) -> Result<Vec<u8>, RegistryError> {
    let json = serde_json::to_vec(&metadata.to_wire())
        .map_err(|e| RegistryError::Protocol(format!("cannot encode publish metadata: {e}")))?;
    let mut out = Vec::with_capacity(PUBLISH_PAYLOAD_MAGIC.len() + 32 + json.len() + tarball.len());
    out.extend_from_slice(PUBLISH_PAYLOAD_MAGIC);
    out.extend_from_slice(json.len().to_string().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&json);
    out.extend_from_slice(tarball);
    Ok(out)
}

/// Decode a publish request body. Tarball-only legacy bodies are accepted and
/// returned with an empty-dependency metadata record for compatibility.
pub fn decode_publish_body<'a>(
    path_name: &str,
    path_version: &str,
    body: &'a [u8],
) -> Result<(PublishMetadata, &'a [u8]), RegistryError> {
    if !body.starts_with(PUBLISH_PAYLOAD_MAGIC) {
        return Ok((
            PublishMetadata {
                name: path_name.to_string(),
                vers: path_version.to_string(),
                deps: Vec::new(),
                features: BTreeMap::new(),
            },
            body,
        ));
    }

    let rest = &body[PUBLISH_PAYLOAD_MAGIC.len()..];
    let Some(len_end) = rest.iter().position(|b| *b == b'\n') else {
        return Err(RegistryError::Protocol(
            "publish metadata sidecar is missing its length".into(),
        ));
    };
    let len_text = std::str::from_utf8(&rest[..len_end]).map_err(|e| {
        RegistryError::Protocol(format!("publish metadata length is not UTF-8: {e}"))
    })?;
    let json_len: usize = len_text.parse().map_err(|_| {
        RegistryError::Protocol(format!("invalid publish metadata length `{len_text}`"))
    })?;
    let after_len = &rest[len_end + 1..];
    if after_len.len() < json_len {
        return Err(RegistryError::Protocol(
            "publish metadata sidecar is truncated".into(),
        ));
    }
    let (json, tarball) = after_len.split_at(json_len);
    let raw: RawPublishMetadata = serde_json::from_slice(json)
        .map_err(|e| RegistryError::Protocol(format!("bad publish metadata sidecar: {e}")))?;
    let metadata = raw.into_metadata()?;
    if metadata.name != path_name || metadata.vers != path_version {
        return Err(RegistryError::Protocol(format!(
            "publish metadata names `{}` v{} but URL names `{path_name}` v{path_version}",
            metadata.name, metadata.vers
        )));
    }
    Ok((metadata, tarball))
}

#[derive(Serialize)]
struct WirePublishMetadata<'a> {
    name: &'a str,
    vers: &'a str,
    deps: Vec<WireIndexDep<'a>>,
    features: &'a BTreeMap<String, Vec<String>>,
}

#[derive(Serialize)]
struct WireIndexDep<'a> {
    name: &'a str,
    req: String,
    optional: bool,
    default_features: bool,
    features: &'a [String],
    registry: &'a Option<String>,
}

impl PublishMetadata {
    fn to_wire(&self) -> WirePublishMetadata<'_> {
        WirePublishMetadata {
            name: &self.name,
            vers: &self.vers,
            deps: self
                .deps
                .iter()
                .map(|d| WireIndexDep {
                    name: &d.name,
                    req: d.req.to_string(),
                    optional: d.optional,
                    default_features: d.default_features,
                    features: &d.features,
                    registry: &d.registry,
                })
                .collect(),
            features: &self.features,
        }
    }
}

// --- Local fixture transport ------------------------------------------------

/// A registry backed by a local directory — the test transport (no network).
///
/// Layout: `<dir>/index/<sharded>/<name>` (JSON-lines) and
/// `<dir>/crates/<name>/<version>.tar.gz` (tarballs).
pub struct LocalRegistry {
    name: String,
    dir: PathBuf,
}

impl LocalRegistry {
    pub fn new(name: impl Into<String>, dir: PathBuf) -> LocalRegistry {
        LocalRegistry {
            name: name.into(),
            dir,
        }
    }
}

impl Registry for LocalRegistry {
    fn name(&self) -> &str {
        &self.name
    }

    fn index(&self, pkg: &str) -> Result<Vec<IndexEntry>, RegistryError> {
        let path = crate::store::index_path(&self.dir.join("index"), pkg);
        match std::fs::read_to_string(&path) {
            Ok(text) => parse_index(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(RegistryError::Transport(e.to_string())),
        }
    }

    fn download(&self, entry: &IndexEntry) -> Result<Vec<u8>, RegistryError> {
        let path = self
            .dir
            .join("crates")
            .join(&entry.name)
            .join(format!("{}.tar.gz", entry.vers));
        std::fs::read(&path)
            .map_err(|e| RegistryError::Transport(format!("reading {}: {e}", path.display())))
    }
}

// --- serde shapes for index lines -------------------------------------------

#[derive(Deserialize)]
struct RawIndexLine {
    name: String,
    vers: String,
    #[serde(default)]
    deps: Vec<RawIndexDep>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
    cksum: String,
    #[serde(default)]
    yanked: bool,
}

#[derive(Deserialize)]
struct RawIndexDep {
    name: String,
    req: String,
    #[serde(default)]
    optional: bool,
    #[serde(default = "yes", rename = "default_features")]
    default_features: bool,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    registry: Option<String>,
}

fn yes() -> bool {
    true
}

impl RawIndexDep {
    fn into_dep(self) -> Result<IndexDep, RegistryError> {
        let req = VersionReq::parse(&self.req)
            .map_err(|e| RegistryError::Protocol(format!("bad dep req `{}`: {e}", self.req)))?;
        Ok(IndexDep {
            name: self.name,
            req,
            optional: self.optional,
            default_features: self.default_features,
            features: self.features,
            registry: self.registry,
        })
    }
}

impl RawIndexLine {
    fn into_entry(self) -> Result<IndexEntry, RegistryError> {
        let vers = Version::parse(&self.vers)
            .map_err(|e| RegistryError::Protocol(format!("bad version `{}`: {e}", self.vers)))?;
        let mut deps = Vec::new();
        for d in self.deps {
            deps.push(d.into_dep()?);
        }
        Ok(IndexEntry {
            name: self.name,
            vers,
            deps,
            features: self.features,
            cksum: self.cksum,
            yanked: self.yanked,
        })
    }
}

#[derive(Deserialize)]
struct RawPublishMetadata {
    name: String,
    vers: String,
    #[serde(default)]
    deps: Vec<RawIndexDep>,
    #[serde(default)]
    features: BTreeMap<String, Vec<String>>,
}

impl RawPublishMetadata {
    fn into_metadata(self) -> Result<PublishMetadata, RegistryError> {
        let mut deps = Vec::new();
        for d in self.deps {
            deps.push(d.into_dep()?);
        }
        Ok(PublishMetadata {
            name: self.name,
            vers: self.vers,
            deps,
            features: self.features,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonlines_index() {
        let text = r#"
{"name":"foo","vers":"1.0.0","deps":[],"cksum":"aa","yanked":false}
{"name":"foo","vers":"1.2.0","deps":[{"name":"bar","req":"^1.0","optional":false,"default_features":true,"features":[]}],"features":{"default":["dep:bar"]},"cksum":"bb","yanked":false}
{"name":"foo","vers":"2.0.0","deps":[],"cksum":"cc","yanked":true}
"#;
        let entries = parse_index(text).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].vers, Version::parse("1.0.0").unwrap());
        assert_eq!(entries[1].deps.len(), 1);
        assert_eq!(entries[1].deps[0].name, "bar");
        assert_eq!(entries[1].features["default"], ["dep:bar"]);
        assert!(entries[2].yanked);
    }

    #[test]
    fn publish_payload_round_trips_metadata_and_tarball() {
        let metadata = PublishMetadata {
            name: "top".into(),
            vers: "1.2.3".into(),
            deps: vec![IndexDep {
                name: "dep".into(),
                req: VersionReq::parse("^1.2").unwrap(),
                optional: true,
                default_features: false,
                features: vec!["tls".into()],
                registry: Some("myco".into()),
            }],
            features: BTreeMap::from([("default".into(), vec!["dep:dep".into()])]),
        };
        let body = encode_publish_body(&metadata, b"tarball-bytes").unwrap();
        let (decoded, tarball) = decode_publish_body("top", "1.2.3", &body).unwrap();
        assert_eq!(decoded, metadata);
        assert_eq!(tarball, b"tarball-bytes");
    }

    #[test]
    fn publish_payload_rejects_metadata_url_mismatch() {
        let metadata = PublishMetadata {
            name: "other".into(),
            vers: "1.2.3".into(),
            deps: Vec::new(),
            features: BTreeMap::new(),
        };
        let body = encode_publish_body(&metadata, b"bytes").unwrap();
        let err = decode_publish_body("top", "1.2.3", &body).unwrap_err();
        assert!(err.to_string().contains("URL names `top`"));
    }

    #[test]
    fn tarball_only_publish_body_is_legacy_empty_metadata() {
        let (decoded, tarball) = decode_publish_body("top", "1.2.3", b"raw-gz").unwrap();
        assert_eq!(decoded.name, "top");
        assert_eq!(decoded.vers, "1.2.3");
        assert!(decoded.deps.is_empty());
        assert!(decoded.features.is_empty());
        assert_eq!(tarball, b"raw-gz");
    }

    #[test]
    fn dl_template_marker_substitution() {
        let t = "https://dl.example.dev/{crate}/{version}/{sha256-checksum}.tar.gz";
        assert_eq!(
            dl_url(t, "serde", "1.2.7", "abcd"),
            "https://dl.example.dev/serde/1.2.7/abcd.tar.gz"
        );
    }

    #[test]
    fn dl_template_without_markers_appends_download() {
        assert_eq!(
            dl_url("https://api.example.dev/", "serde", "1.2.7", "x"),
            "https://api.example.dev/serde/1.2.7/download"
        );
    }

    #[test]
    fn local_registry_reads_index_and_tarball() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("otter_localreg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // index/3/f/foo
        let idx = crate::store::index_path(&dir.join("index"), "foo");
        std::fs::create_dir_all(idx.parent().unwrap()).unwrap();
        std::fs::write(
            &idx,
            "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"aa\",\"yanked\":false}\n",
        )
        .unwrap();
        // crates/foo/1.0.0.tar.gz (any bytes; download doesn't verify)
        let tdir = dir.join("crates").join("foo");
        std::fs::create_dir_all(&tdir).unwrap();
        let mut f = std::fs::File::create(tdir.join("1.0.0.tar.gz")).unwrap();
        f.write_all(b"tarball-bytes").unwrap();

        let reg = LocalRegistry::new("test", dir.clone());
        let entries = reg.index("foo").unwrap();
        assert_eq!(entries.len(), 1);
        let bytes = reg.download(&entries[0]).unwrap();
        assert_eq!(bytes, b"tarball-bytes");
        assert!(reg.index("missing").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
