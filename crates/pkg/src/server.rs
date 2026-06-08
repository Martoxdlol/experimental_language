//! A minimal sparse-HTTP registry server (`docs/23` §7).
//!
//! Serves exactly the protocol [`crate::registry::HttpRegistry`] speaks —
//! `config.json`, the sharded sparse index, tarball downloads, and the
//! `api/v1` write/query endpoints (`search`/`publish`/`yank`) — over plain
//! HTTP/1.1 on a [`TcpListener`], with **no external dependencies**.
//!
//! It is two things at once: a self-hostable private registry, and the fixture
//! the client's *live* network round-trips are exercised against (the offline
//! [`crate::registry::LocalRegistry`] proves resolution logic; this proves the
//! wire transport).
//!
//! The on-disk layout matches `LocalRegistry`: `<dir>/index/<sharded>/<name>`
//! holds the JSON-lines index, and `<dir>/crates/<name>/<version>.tar.gz` holds
//! the tarballs. `publish` writes both; the checksum is computed server-side
//! from the uploaded bytes.
//!
//! Scope: this is a complete, correct implementation of the endpoints the
//! client uses, sufficient to host a registry and to round-trip every client
//! operation. Published index lines are written from the client's metadata
//! sidecar, so dependency edges are available to later resolver runs.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::registry::{IndexEntry, decode_publish_body, parse_index};
use crate::store::{index_path, sha256_hex};
use crate::version::Version;

/// A running registry server. Drop or call [`ServerHandle::shutdown`] to stop.
pub struct ServerHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ServerHandle {
    /// The bound socket address (host:port).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The base URL clients connect to (e.g. `http://127.0.0.1:54321`).
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Wait until the server thread exits. Since the accept loop only stops on
    /// [`ServerHandle::shutdown`] or a fatal listener error, this parks the
    /// caller for the server's lifetime — the right behavior for a foreground
    /// `otter_fusion serve` process (terminate it with a signal).
    pub fn wait(mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// Stop the server and wait for its thread to exit.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Unblock a thread parked in `accept` by poking the socket.
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Start a registry server backed by `dir`, listening on an ephemeral
/// `127.0.0.1` port. If `token` is `Some`, write operations (`publish`/`yank`)
/// require `Authorization: Bearer <token>`; reads are always open.
pub fn serve(dir: PathBuf, token: Option<String>) -> std::io::Result<ServerHandle> {
    serve_on("127.0.0.1:0", dir, token)
}

/// Like [`serve`] but binds an explicit address (e.g. `0.0.0.0:8080` to host a
/// real private registry on the network).
pub fn serve_on(bind: &str, dir: PathBuf, token: Option<String>) -> std::io::Result<ServerHandle> {
    let listener = TcpListener::bind(bind)?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let ctx = Arc::new(Ctx {
        dir,
        token,
        base: format!("http://{addr}"),
    });

    let stop_thread = stop.clone();
    let join = std::thread::spawn(move || {
        loop {
            if stop_thread.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _peer)) => {
                    // Use per-connection stream mode for this request.
                    let _ = stream.set_nonblocking(false);
                    let ctx = ctx.clone();
                    // Serve sequentially: registry traffic is light and
                    // publish→read ordering must be observable in-order.
                    handle_conn(&mut stream, &ctx);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(4));
                }
                Err(_) => break,
            }
        }
    });

    Ok(ServerHandle {
        addr,
        stop,
        join: Some(join),
    })
}

/// Shared per-server state.
struct Ctx {
    dir: PathBuf,
    token: Option<String>,
    base: String,
}

/// A parsed HTTP/1.1 request (only the parts the registry needs).
struct Request {
    method: String,
    path: String,
    query: String,
    auth: Option<String>,
    body: Vec<u8>,
}

fn handle_conn(stream: &mut TcpStream, ctx: &Ctx) {
    let req = match read_request(stream) {
        Some(r) => r,
        None => return,
    };
    let resp = route(&req, ctx);
    let _ = write_response(stream, &resp);
}

/// Parse the request line, headers, and (length-delimited) body.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };

    let mut content_length = 0usize;
    let mut auth = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            } else if name == "authorization" {
                auth = value.strip_prefix("Bearer ").map(|t| t.to_string());
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path,
        query,
        auth,
        body,
    })
}

/// An HTTP response to send back.
struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn text(status: u16, reason: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            reason,
            content_type: "text/plain",
            body: body.into(),
        }
    }
    fn json(body: String) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "application/json",
            body: body.into_bytes(),
        }
    }
    fn bytes(body: Vec<u8>) -> Response {
        Response {
            status: 200,
            reason: "OK",
            content_type: "application/octet-stream",
            body,
        }
    }
    fn not_found() -> Response {
        Response::text(404, "Not Found", "not found")
    }
    fn unauthorized() -> Response {
        Response::text(401, "Unauthorized", "missing or invalid token")
    }
}

fn write_response(stream: &mut TcpStream, resp: &Response) -> std::io::Result<()> {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        resp.reason,
        resp.body.len(),
        resp.content_type,
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn route(req: &Request, ctx: &Ctx) -> Response {
    let path = req.path.as_str();
    if req.method == "GET" && path == "/config.json" {
        return config_json(ctx);
    }
    if path == "/api/v1/crates" && req.method == "GET" {
        return search(req, ctx);
    }
    if let Some(rest) = path.strip_prefix("/api/v1/crates/") {
        // `<name>/<version>/publish` (PUT) or `<name>/<version>/yank` (DELETE)
        let segs: Vec<&str> = rest.split('/').collect();
        if segs.len() == 3 {
            let (name, version, op) = (segs[0], segs[1], segs[2]);
            if !authed(req, ctx) {
                return Response::unauthorized();
            }
            return match (req.method.as_str(), op) {
                ("PUT", "publish") => publish(ctx, name, version, &req.body),
                ("DELETE", "yank") => set_yanked(ctx, name, version, true),
                ("DELETE", "unyank") => set_yanked(ctx, name, version, false),
                _ => Response::not_found(),
            };
        }
        return Response::not_found();
    }
    if req.method == "GET" {
        if let Some(rest) = path.strip_prefix("/crates/") {
            return download(ctx, rest);
        }
        // Anything else is a sparse-index lookup: the package name is the last
        // path segment (the shard prefix is recomputed from the name).
        if let Some(name) = path.rsplit('/').next().filter(|s| !s.is_empty()) {
            return index(ctx, name);
        }
    }
    Response::not_found()
}

fn authed(req: &Request, ctx: &Ctx) -> bool {
    match &ctx.token {
        None => true,
        Some(expected) => req.auth.as_deref() == Some(expected.as_str()),
    }
}

fn config_json(ctx: &Ctx) -> Response {
    let dl = format!(
        "{}/crates/{{crate}}/{{version}}/{{sha256-checksum}}.tar.gz",
        ctx.base
    );
    let v = serde_json::json!({
        "dl": dl,
        "api": ctx.base,
        "auth-required": false,
    });
    Response::json(v.to_string())
}

fn index(ctx: &Ctx, name: &str) -> Response {
    let path = index_path(&ctx.dir.join("index"), name);
    match std::fs::read_to_string(&path) {
        Ok(text) => Response::text(200, "OK", text),
        Err(_) => Response::not_found(),
    }
}

fn download(ctx: &Ctx, rest: &str) -> Response {
    // `<name>/<version>/<cksum>.tar.gz`
    let segs: Vec<&str> = rest.split('/').collect();
    if segs.len() < 3 {
        return Response::not_found();
    }
    let (name, version) = (segs[0], segs[1]);
    let path = ctx
        .dir
        .join("crates")
        .join(name)
        .join(format!("{version}.tar.gz"));
    match std::fs::read(&path) {
        Ok(bytes) => Response::bytes(bytes),
        Err(_) => Response::not_found(),
    }
}

fn publish(ctx: &Ctx, name: &str, version: &str, tarball: &[u8]) -> Response {
    if Version::parse(version).is_err() {
        return Response::text(400, "Bad Request", "invalid version");
    }
    let (metadata, tarball) = match decode_publish_body(name, version, tarball) {
        Ok(decoded) => decoded,
        Err(e) => return Response::text(400, "Bad Request", e.to_string()),
    };
    let cksum = sha256_hex(tarball);

    // Store the tarball.
    let crate_dir = ctx.dir.join("crates").join(name);
    if std::fs::create_dir_all(&crate_dir).is_err() {
        return Response::text(500, "Internal Server Error", "cannot create crate dir");
    }
    if std::fs::write(crate_dir.join(format!("{version}.tar.gz")), tarball).is_err() {
        return Response::text(500, "Internal Server Error", "cannot write tarball");
    }

    // Upsert the index line.
    let path = index_path(&ctx.dir.join("index"), name);
    let mut entries = read_entries(&path);
    let ver = Version::parse(version).unwrap();
    if let Some(e) = entries.iter_mut().find(|e| e.vers == ver) {
        e.cksum = cksum.clone();
        e.yanked = false;
        e.deps = metadata.deps;
        e.features = metadata.features;
    } else {
        entries.push(IndexEntry {
            name: name.to_string(),
            vers: ver,
            deps: metadata.deps,
            features: metadata.features,
            cksum: cksum.clone(),
            yanked: false,
        });
    }
    if write_entries(&path, &entries).is_err() {
        return Response::text(500, "Internal Server Error", "cannot write index");
    }
    Response::json(serde_json::json!({ "ok": true, "cksum": cksum }).to_string())
}

fn set_yanked(ctx: &Ctx, name: &str, version: &str, yanked: bool) -> Response {
    let ver = match Version::parse(version) {
        Ok(v) => v,
        Err(_) => return Response::text(400, "Bad Request", "invalid version"),
    };
    let path = index_path(&ctx.dir.join("index"), name);
    let mut entries = read_entries(&path);
    let Some(e) = entries.iter_mut().find(|e| e.vers == ver) else {
        return Response::not_found();
    };
    e.yanked = yanked;
    if write_entries(&path, &entries).is_err() {
        return Response::text(500, "Internal Server Error", "cannot write index");
    }
    Response::json(serde_json::json!({ "ok": true }).to_string())
}

fn search(req: &Request, ctx: &Ctx) -> Response {
    let mut query = String::new();
    let mut limit = 10usize;
    for pair in req.query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            match k {
                "q" => query = urldecode(v),
                "per_page" => limit = v.parse().unwrap_or(10),
                _ => {}
            }
        }
    }
    let needle = query.to_lowercase();

    // Walk every index file and collect the max non-yanked version per name.
    let mut hits: Vec<(String, Version)> = Vec::new();
    let index_root = ctx.dir.join("index");
    for entries in walk_index(&index_root) {
        if entries.is_empty() {
            continue;
        }
        let name = entries[0].name.clone();
        if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
            continue;
        }
        if let Some(max) = entries
            .iter()
            .filter(|e| !e.yanked)
            .map(|e| e.vers.clone())
            .max()
        {
            hits.push((name, max));
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0));
    hits.truncate(limit);

    let crates: Vec<_> = hits
        .into_iter()
        .map(|(name, vers)| {
            serde_json::json!({ "name": name, "max_version": vers.to_string(), "description": "" })
        })
        .collect();
    Response::json(serde_json::json!({ "crates": crates }).to_string())
}

// ---------------------------------------------------------------------------
// Index file helpers
// ---------------------------------------------------------------------------

fn read_entries(path: &std::path::Path) -> Vec<IndexEntry> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_index(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn write_entries(path: &std::path::Path, entries: &[IndexEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for e in entries {
        text.push_str(&index_line(e));
        text.push('\n');
    }
    std::fs::write(path, text)
}

/// Serialize one `IndexEntry` as a sparse-index JSON line (round-trips through
/// [`parse_index`]).
fn index_line(e: &IndexEntry) -> String {
    let deps: Vec<serde_json::Value> = e
        .deps
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "req": d.req.to_string(),
                "optional": d.optional,
                "default_features": d.default_features,
                "features": d.features,
                "registry": d.registry,
            })
        })
        .collect();
    serde_json::json!({
        "name": e.name,
        "vers": e.vers.to_string(),
        "deps": deps,
        "features": e.features,
        "cksum": e.cksum,
        "yanked": e.yanked,
    })
    .to_string()
}

/// Recursively read every index file under `root`, returning each file's
/// parsed entries.
fn walk_index(root: &std::path::Path) -> Vec<Vec<IndexEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(entries) = parse_index(&text) {
                    out.push(entries);
                }
            }
        }
    }
    out
}

/// Minimal percent-decoding for a query-string value (`+` → space).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{HttpRegistry, IndexDep, PublishMetadata, Registry};
    use crate::version::VersionReq;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("otter_server_{tag}_{n}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn publish_records_metadata_sidecar_deps_in_index() {
        let dir = temp_dir("publish_deps");
        let server = serve(dir.clone(), Some("secret".into())).unwrap();
        let reg = HttpRegistry::connect(
            "public",
            &format!("sparse+{}", server.base_url()),
            Some("secret".into()),
        )
        .unwrap();
        let metadata = PublishMetadata {
            name: "top".into(),
            vers: "1.2.3".into(),
            deps: vec![IndexDep {
                name: "bottom".into(),
                req: VersionReq::parse("^0.4").unwrap(),
                optional: false,
                default_features: false,
                features: vec!["tls".into()],
                registry: Some("myco".into()),
            }],
            features: std::collections::BTreeMap::from([(
                "default".into(),
                vec!["dep:bottom".into()],
            )]),
        };

        reg.publish("top", "1.2.3", b"tarball", &metadata).unwrap();

        let entries = reg.index("top").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].deps.len(), 1);
        assert_eq!(entries[0].deps[0].name, "bottom");
        assert_eq!(entries[0].deps[0].req.to_string(), "^0.4");
        assert!(!entries[0].deps[0].default_features);
        assert_eq!(entries[0].deps[0].features, ["tls"]);
        assert_eq!(entries[0].deps[0].registry.as_deref(), Some("myco"));
        assert_eq!(entries[0].features["default"], ["dep:bottom"]);
        assert_eq!(reg.download(&entries[0]).unwrap(), b"tarball");

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_upsert_replaces_metadata_deps() {
        let dir = temp_dir("publish_upsert");
        let server = serve(dir.clone(), Some("secret".into())).unwrap();
        let reg = HttpRegistry::connect(
            "public",
            &format!("sparse+{}", server.base_url()),
            Some("secret".into()),
        )
        .unwrap();
        let first = PublishMetadata {
            name: "top".into(),
            vers: "1.0.0".into(),
            deps: vec![IndexDep {
                name: "old".into(),
                req: VersionReq::parse("^1").unwrap(),
                optional: false,
                default_features: true,
                features: Vec::new(),
                registry: None,
            }],
            features: std::collections::BTreeMap::from([("old-feature".into(), Vec::new())]),
        };
        let second = PublishMetadata {
            name: "top".into(),
            vers: "1.0.0".into(),
            deps: vec![IndexDep {
                name: "new".into(),
                req: VersionReq::parse("^2").unwrap(),
                optional: true,
                default_features: true,
                features: Vec::new(),
                registry: None,
            }],
            features: std::collections::BTreeMap::from([(
                "new-feature".into(),
                vec!["dep:new".into()],
            )]),
        };

        reg.publish("top", "1.0.0", b"first", &first).unwrap();
        reg.publish("top", "1.0.0", b"second", &second).unwrap();

        let entries = reg.index("top").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].deps.len(), 1);
        assert_eq!(entries[0].deps[0].name, "new");
        assert!(entries[0].deps[0].optional);
        assert!(entries[0].features.contains_key("new-feature"));
        assert!(!entries[0].features.contains_key("old-feature"));
        assert_eq!(reg.download(&entries[0]).unwrap(), b"second");

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
