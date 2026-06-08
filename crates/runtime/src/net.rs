//! Runtime networking hooks for `std:net`.
//!
//! The public network value contracts live in Otter Fusion source. Runtime
//! hooks only expose provider-backed host operations using compact string
//! payloads decoded by the stdlib layer.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::{
    Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket as OsUdpSocket,
};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration as StdDuration;

fn push_field(out: &mut String, value: &str) {
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

fn encode_success_string(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 1);
    out.push('0');
    out.push_str(payload);
    out
}

fn encode_error_string(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    let mut out = String::with_capacity(message.len() + 1);
    out.push('1');
    out.push_str(&message);
    out
}

fn timeout_from_abi(nanos: i64, present: i64) -> std::io::Result<Option<StdDuration>> {
    if present == 0 {
        return Ok(None);
    }
    if nanos < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid socket timeout",
        ));
    }
    Ok(Some(StdDuration::from_nanos(nanos as u64)))
}

fn encode_timeout_result_string(result: std::io::Result<Option<StdDuration>>) -> String {
    match result {
        Ok(Some(timeout)) => match i64::try_from(timeout.as_nanos()) {
            Ok(nanos) => encode_success_string(&nanos.to_string()),
            Err(_) => encode_error_string("socket timeout exceeds Duration range"),
        },
        Ok(None) => encode_success_string("n"),
        Err(err) => encode_error_string(err),
    }
}

fn bytes_hex_payload(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut payload = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        payload.push(HEX[(byte >> 4) as usize] as char);
        payload.push(HEX[(byte & 0x0f) as usize] as char);
    }
    payload
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_hex_bytes(hex: &str) -> std::io::Result<Vec<u8>> {
    let bytes = hex.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "odd-length network byte payload",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = decode_hex_digit(bytes[i]).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid hexadecimal digit in network byte payload",
            )
        })?;
        let lo = decode_hex_digit(bytes[i + 1]).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid hexadecimal digit in network byte payload",
            )
        })?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn parse_ipv4_addr(text: &str, label: &str) -> std::io::Result<Ipv4Addr> {
    text.parse::<Ipv4Addr>().map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid IPv4 multicast {label}: {err}"),
        )
    })
}

fn parse_ipv6_addr(text: &str, label: &str) -> std::io::Result<Ipv6Addr> {
    text.parse::<Ipv6Addr>().map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid IPv6 multicast {label}: {err}"),
        )
    })
}

type SharedStream = Arc<Mutex<TcpStream>>;
type SharedListener = Arc<Mutex<TcpListener>>;
type SharedUdpSocket = Arc<Mutex<OsUdpSocket>>;

static NEXT_STREAM_HANDLE: AtomicI64 = AtomicI64::new(1);
static NEXT_LISTENER_HANDLE: AtomicI64 = AtomicI64::new(1);
static NEXT_UDP_HANDLE: AtomicI64 = AtomicI64::new(1);
static STREAMS: OnceLock<Mutex<HashMap<i64, SharedStream>>> = OnceLock::new();
static LISTENERS: OnceLock<Mutex<HashMap<i64, SharedListener>>> = OnceLock::new();
static UDP_SOCKETS: OnceLock<Mutex<HashMap<i64, SharedUdpSocket>>> = OnceLock::new();

fn stream_registry() -> &'static Mutex<HashMap<i64, SharedStream>> {
    STREAMS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn listener_registry() -> &'static Mutex<HashMap<i64, SharedListener>> {
    LISTENERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn udp_registry() -> &'static Mutex<HashMap<i64, SharedUdpSocket>> {
    UDP_SOCKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn register_stream(stream: TcpStream) -> i64 {
    let handle = NEXT_STREAM_HANDLE.fetch_add(1, Ordering::Relaxed);
    stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .insert(handle, Arc::new(Mutex::new(stream)));
    handle
}

#[cfg(test)]
pub(crate) fn test_stream_handle_registered(handle: i64) -> bool {
    stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .contains_key(&handle)
}

fn register_listener(listener: TcpListener) -> i64 {
    let handle = NEXT_LISTENER_HANDLE.fetch_add(1, Ordering::Relaxed);
    listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .insert(handle, Arc::new(Mutex::new(listener)));
    handle
}

#[cfg(test)]
pub(crate) fn test_listener_handle_registered(handle: i64) -> bool {
    listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .contains_key(&handle)
}

fn register_udp_socket(socket: OsUdpSocket) -> i64 {
    let handle = NEXT_UDP_HANDLE.fetch_add(1, Ordering::Relaxed);
    udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .insert(handle, Arc::new(Mutex::new(socket)));
    handle
}

#[cfg(test)]
pub(crate) fn test_udp_handle_registered(handle: i64) -> bool {
    udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .contains_key(&handle)
}

fn stream_handle(handle: i64) -> std::io::Result<SharedStream> {
    stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .get(&handle)
        .cloned()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid TCP stream handle",
            )
        })
}

fn listener_handle(handle: i64) -> std::io::Result<SharedListener> {
    listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .get(&handle)
        .cloned()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid TCP listener handle",
            )
        })
}

fn udp_handle(handle: i64) -> std::io::Result<SharedUdpSocket> {
    udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .get(&handle)
        .cloned()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid UDP socket handle",
            )
        })
}

fn drop_network_handle_native_wait<T>(handle: T) {
    crate::gc::native_wait(|| drop(handle));
}

fn bind_tcp_listener_native_wait(addr: SocketAddr) -> std::io::Result<TcpListener> {
    crate::gc::native_wait(|| TcpListener::bind(addr))
}

fn bind_udp_socket_native_wait(addr: SocketAddr) -> std::io::Result<OsUdpSocket> {
    crate::gc::native_wait(|| OsUdpSocket::bind(addr))
}

fn with_tcp_stream_native_wait<T>(
    handle: i64,
    f: impl FnOnce(&TcpStream) -> std::io::Result<T>,
) -> std::io::Result<T> {
    stream_handle(handle).and_then(|stream| {
        crate::gc::native_wait(|| {
            let stream = stream.lock().expect("network stream poisoned");
            f(&stream)
        })
    })
}

fn with_tcp_listener_native_wait<T>(
    handle: i64,
    f: impl FnOnce(&TcpListener) -> std::io::Result<T>,
) -> std::io::Result<T> {
    listener_handle(handle).and_then(|listener| {
        crate::gc::native_wait(|| {
            let listener = listener.lock().expect("network listener poisoned");
            f(&listener)
        })
    })
}

fn with_udp_socket_native_wait<T>(
    handle: i64,
    f: impl FnOnce(&OsUdpSocket) -> std::io::Result<T>,
) -> std::io::Result<T> {
    udp_handle(handle).and_then(|socket| {
        crate::gc::native_wait(|| {
            let socket = socket.lock().expect("network UDP socket poisoned");
            f(&socket)
        })
    })
}

fn resolve_host(host: &str) -> Result<Vec<String>, String> {
    let mut unique = BTreeSet::new();
    (host, 0u16)
        .to_socket_addrs()
        .map_err(|err| err.to_string())?
        .for_each(|addr| {
            unique.insert(addr.ip().to_string());
        });
    Ok(unique.into_iter().collect())
}

pub(crate) fn net_resolve_encoded(host: String) -> String {
    let result = crate::gc::native_wait(|| resolve_host(&host));
    let payload = match result {
        Ok(addrs) => {
            let mut out = String::from("0");
            for addr in addrs {
                push_field(&mut out, &addr);
            }
            out
        }
        Err(err) => format!("1{err}"),
    };
    payload
}

pub(crate) fn net_tcp_connect_encoded(addr: String) -> String {
    let result = addr
        .parse::<SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|addr| {
            crate::gc::native_wait(|| TcpStream::connect(addr)).map_err(|err| err.to_string())
        });
    match result {
        Ok(stream) => encode_success_string(&register_stream(stream).to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_connect_timeout_encoded(addr: String, nanos: i64) -> String {
    let result = timeout_from_abi(nanos, 1)
        .and_then(|timeout| {
            timeout.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid socket timeout")
            })
        })
        .and_then(|timeout| {
            let addr = addr.parse::<SocketAddr>().map_err(|err| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
            })?;
            crate::gc::native_wait(|| TcpStream::connect_timeout(&addr, timeout))
        });
    match result {
        Ok(stream) => encode_success_string(&register_stream(stream).to_string()),
        Err(err) => encode_error_string(err),
    }
}

/// Release a TCP stream handle from the runtime registry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_release(handle: i64) {
    let removed = stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .remove(&handle);
    if let Some(stream) = removed {
        drop_network_handle_native_wait(stream);
    }
}

pub(crate) fn net_tcp_stream_close_encoded(handle: i64) -> String {
    let removed = stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .remove(&handle);
    match removed {
        Some(stream) => {
            drop_network_handle_native_wait(stream);
            encode_success_string("")
        }
        None => encode_error_string("invalid TCP stream handle"),
    }
}

pub(crate) fn net_tcp_stream_peer_addr_encoded(handle: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| {
        stream.peer_addr().map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success_string(&addr),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_local_addr_encoded(handle: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| {
        stream.local_addr().map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success_string(&addr),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_take_error_encoded(handle: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| stream.take_error()) {
        Ok(Some(err)) => encode_success_string(&format!("1{err}")),
        Ok(None) => encode_success_string("0"),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_set_nodelay_encoded(handle: i64, on: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| stream.set_nodelay(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_nodelay_encoded(handle: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| {
        stream.nodelay().map(|on| if on { 1 } else { 0 })
    }) {
        Ok(on) => encode_success_string(&on.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_set_nonblocking_encoded(handle: i64, on: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| stream.set_nonblocking(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_read_timeout_encoded(handle: i64) -> String {
    encode_timeout_result_string(with_tcp_stream_native_wait(handle, |stream| {
        stream.read_timeout()
    }))
}

pub(crate) fn net_tcp_stream_set_read_timeout_encoded(
    handle: i64,
    nanos: i64,
    present: i64,
) -> String {
    match timeout_from_abi(nanos, present).and_then(|timeout| {
        with_tcp_stream_native_wait(handle, |stream| stream.set_read_timeout(timeout))
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_write_timeout_encoded(handle: i64) -> String {
    encode_timeout_result_string(with_tcp_stream_native_wait(handle, |stream| {
        stream.write_timeout()
    }))
}

pub(crate) fn net_tcp_stream_set_write_timeout_encoded(
    handle: i64,
    nanos: i64,
    present: i64,
) -> String {
    match timeout_from_abi(nanos, present).and_then(|timeout| {
        with_tcp_stream_native_wait(handle, |stream| stream.set_write_timeout(timeout))
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_ttl_encoded(handle: i64) -> String {
    match with_tcp_stream_native_wait(handle, |stream| stream.ttl()) {
        Ok(ttl) => encode_success_string(&ttl.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_set_ttl_encoded(handle: i64, ttl: i64) -> String {
    let ttl = match u32::try_from(ttl) {
        Ok(ttl) => ttl,
        Err(_) => return encode_error_string("invalid socket TTL"),
    };
    match with_tcp_stream_native_wait(handle, |stream| stream.set_ttl(ttl)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_peek_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid peek length");
    }
    let result = stream_handle(handle).and_then(|stream| {
        let mut buf = vec![0u8; count as usize];
        let n = crate::gc::native_wait(|| {
            stream
                .lock()
                .expect("network stream poisoned")
                .peek(&mut buf)
        })?;
        buf.truncate(n);
        Ok(buf)
    });
    match result {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_read_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid read length");
    }
    let result = stream_handle(handle).and_then(|stream| {
        let mut buf = vec![0u8; count as usize];
        let n = crate::gc::native_wait(|| {
            stream
                .lock()
                .expect("network stream poisoned")
                .read(&mut buf)
        })?;
        buf.truncate(n);
        Ok(buf)
    });
    match result {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_read_to_end_encoded(handle: i64) -> String {
    let result = stream_handle(handle).and_then(|stream| {
        let mut buf = Vec::new();
        crate::gc::native_wait(|| {
            stream
                .lock()
                .expect("network stream poisoned")
                .read_to_end(&mut buf)
        })?;
        Ok(buf)
    });
    match result {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_write_encoded(handle: i64, contents_hex: String) -> String {
    let result = decode_hex_bytes(&contents_hex).and_then(|bytes| {
        stream_handle(handle).and_then(|stream| {
            crate::gc::native_wait(|| {
                stream
                    .lock()
                    .expect("network stream poisoned")
                    .write(&bytes)
            })
            .map(|n| n as i64)
        })
    });
    match result {
        Ok(n) => encode_success_string(&n.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_write_all_encoded(handle: i64, contents_hex: String) -> String {
    let result = decode_hex_bytes(&contents_hex).and_then(|bytes| {
        stream_handle(handle).and_then(|stream| {
            crate::gc::native_wait(|| {
                stream
                    .lock()
                    .expect("network stream poisoned")
                    .write_all(&bytes)
            })
        })
    });
    match result {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_stream_flush_encoded(handle: i64) -> String {
    let result = stream_handle(handle).and_then(|stream| {
        crate::gc::native_wait(|| stream.lock().expect("network stream poisoned").flush())
    });
    match result {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_listener_bind_encoded(addr: String) -> String {
    match addr
        .parse::<SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|addr| bind_tcp_listener_native_wait(addr).map_err(|err| err.to_string()))
    {
        Ok(listener) => encode_success_string(&register_listener(listener).to_string()),
        Err(err) => encode_error_string(err),
    }
}

/// Release a TCP listener handle from the runtime registry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_release(handle: i64) {
    let removed = listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .remove(&handle);
    if let Some(listener) = removed {
        drop_network_handle_native_wait(listener);
    }
}

pub(crate) fn net_tcp_listener_close_encoded(handle: i64) -> String {
    let removed = listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .remove(&handle);
    match removed {
        Some(listener) => {
            drop_network_handle_native_wait(listener);
            encode_success_string("")
        }
        None => encode_error_string("invalid TCP listener handle"),
    }
}

pub(crate) fn net_tcp_listener_accept_encoded(handle: i64) -> String {
    let result = listener_handle(handle).and_then(|listener| {
        let (stream, peer) = crate::gc::native_wait(|| {
            listener.lock().expect("network listener poisoned").accept()
        })?;
        Ok((register_stream(stream), peer.to_string()))
    });
    match result {
        Ok((stream_handle, peer)) => {
            let mut payload = String::new();
            push_field(&mut payload, &stream_handle.to_string());
            push_field(&mut payload, &peer);
            encode_success_string(&payload)
        }
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_listener_local_addr_encoded(handle: i64) -> String {
    match with_tcp_listener_native_wait(handle, |listener| {
        listener.local_addr().map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success_string(&addr),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_listener_take_error_encoded(handle: i64) -> String {
    match with_tcp_listener_native_wait(handle, |listener| listener.take_error()) {
        Ok(Some(err)) => encode_success_string(&format!("1{err}")),
        Ok(None) => encode_success_string("0"),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_listener_set_nonblocking_encoded(handle: i64, on: i64) -> String {
    match with_tcp_listener_native_wait(handle, |listener| listener.set_nonblocking(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_listener_ttl_encoded(handle: i64) -> String {
    match with_tcp_listener_native_wait(handle, |listener| listener.ttl()) {
        Ok(ttl) => encode_success_string(&ttl.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_tcp_listener_set_ttl_encoded(handle: i64, ttl: i64) -> String {
    let ttl = match u32::try_from(ttl) {
        Ok(ttl) => ttl,
        Err(_) => return encode_error_string("invalid socket TTL"),
    };
    match with_tcp_listener_native_wait(handle, |listener| listener.set_ttl(ttl)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_bind_encoded(addr: String) -> String {
    match addr
        .parse::<SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|addr| bind_udp_socket_native_wait(addr).map_err(|err| err.to_string()))
    {
        Ok(socket) => encode_success_string(&register_udp_socket(socket).to_string()),
        Err(err) => encode_error_string(err),
    }
}

/// Release a UDP socket handle from the runtime registry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_release(handle: i64) {
    let removed = udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .remove(&handle);
    if let Some(socket) = removed {
        drop_network_handle_native_wait(socket);
    }
}

pub(crate) fn net_udp_close_encoded(handle: i64) -> String {
    let removed = udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .remove(&handle);
    match removed {
        Some(socket) => {
            drop_network_handle_native_wait(socket);
            encode_success_string("")
        }
        None => encode_error_string("invalid UDP socket handle"),
    }
}

pub(crate) fn net_udp_local_addr_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| {
        socket.local_addr().map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success_string(&addr),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_connect_encoded(handle: i64, addr: String) -> String {
    match addr
        .parse::<SocketAddr>()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string()))
        .and_then(|addr| with_udp_socket_native_wait(handle, |socket| socket.connect(addr)))
    {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_peer_addr_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| {
        socket.peer_addr().map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success_string(&addr),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_take_error_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.take_error()) {
        Ok(Some(err)) => encode_success_string(&format!("1{err}")),
        Ok(None) => encode_success_string("0"),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_set_nonblocking_encoded(handle: i64, on: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.set_nonblocking(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_read_timeout_encoded(handle: i64) -> String {
    encode_timeout_result_string(with_udp_socket_native_wait(handle, |socket| {
        socket.read_timeout()
    }))
}

pub(crate) fn net_udp_set_read_timeout_encoded(handle: i64, nanos: i64, present: i64) -> String {
    match timeout_from_abi(nanos, present).and_then(|timeout| {
        with_udp_socket_native_wait(handle, |socket| socket.set_read_timeout(timeout))
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_write_timeout_encoded(handle: i64) -> String {
    encode_timeout_result_string(with_udp_socket_native_wait(handle, |socket| {
        socket.write_timeout()
    }))
}

pub(crate) fn net_udp_set_write_timeout_encoded(handle: i64, nanos: i64, present: i64) -> String {
    match timeout_from_abi(nanos, present).and_then(|timeout| {
        with_udp_socket_native_wait(handle, |socket| socket.set_write_timeout(timeout))
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_ttl_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.ttl()) {
        Ok(ttl) => encode_success_string(&ttl.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_set_ttl_encoded(handle: i64, ttl: i64) -> String {
    let ttl = match u32::try_from(ttl) {
        Ok(ttl) => ttl,
        Err(_) => return encode_error_string("invalid socket TTL"),
    };
    match with_udp_socket_native_wait(handle, |socket| socket.set_ttl(ttl)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_broadcast_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| {
        socket.broadcast().map(|on| if on { 1 } else { 0 })
    }) {
        Ok(on) => encode_success_string(&on.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_set_broadcast_encoded(handle: i64, on: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.set_broadcast(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_multicast_loop_v4_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| {
        socket.multicast_loop_v4().map(|on| if on { 1 } else { 0 })
    }) {
        Ok(on) => encode_success_string(&on.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_set_multicast_loop_v4_encoded(handle: i64, on: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.set_multicast_loop_v4(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_multicast_loop_v6_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| {
        socket.multicast_loop_v6().map(|on| if on { 1 } else { 0 })
    }) {
        Ok(on) => encode_success_string(&on.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_set_multicast_loop_v6_encoded(handle: i64, on: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.set_multicast_loop_v6(on != 0)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_multicast_ttl_v4_encoded(handle: i64) -> String {
    match with_udp_socket_native_wait(handle, |socket| socket.multicast_ttl_v4()) {
        Ok(ttl) => encode_success_string(&ttl.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_set_multicast_ttl_v4_encoded(handle: i64, ttl: i64) -> String {
    let ttl = match u32::try_from(ttl) {
        Ok(ttl) => ttl,
        Err(_) => return encode_error_string("invalid IPv4 multicast TTL"),
    };
    match with_udp_socket_native_wait(handle, |socket| socket.set_multicast_ttl_v4(ttl)) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_join_multicast_v4_encoded(
    handle: i64,
    group: String,
    interface: String,
) -> String {
    match parse_ipv4_addr(&group, "group").and_then(|group| {
        let interface = parse_ipv4_addr(&interface, "interface")?;
        with_udp_socket_native_wait(handle, |socket| {
            socket.join_multicast_v4(&group, &interface)
        })
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_leave_multicast_v4_encoded(
    handle: i64,
    group: String,
    interface: String,
) -> String {
    match parse_ipv4_addr(&group, "group").and_then(|group| {
        let interface = parse_ipv4_addr(&interface, "interface")?;
        with_udp_socket_native_wait(handle, |socket| {
            socket.leave_multicast_v4(&group, &interface)
        })
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_join_multicast_v6_encoded(
    handle: i64,
    group: String,
    interface: i64,
) -> String {
    let interface = match u32::try_from(interface) {
        Ok(interface) => interface,
        Err(_) => return encode_error_string("invalid IPv6 multicast interface index"),
    };
    match parse_ipv6_addr(&group, "group").and_then(|group| {
        with_udp_socket_native_wait(handle, |socket| socket.join_multicast_v6(&group, interface))
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_leave_multicast_v6_encoded(
    handle: i64,
    group: String,
    interface: i64,
) -> String {
    let interface = match u32::try_from(interface) {
        Ok(interface) => interface,
        Err(_) => return encode_error_string("invalid IPv6 multicast interface index"),
    };
    match parse_ipv6_addr(&group, "group").and_then(|group| {
        with_udp_socket_native_wait(handle, |socket| {
            socket.leave_multicast_v6(&group, interface)
        })
    }) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_send_encoded(handle: i64, contents_hex: String) -> String {
    let result = decode_hex_bytes(&contents_hex).and_then(|bytes| {
        udp_handle(handle).and_then(|socket| {
            crate::gc::native_wait(|| {
                socket
                    .lock()
                    .expect("network UDP socket poisoned")
                    .send(&bytes)
            })
            .map(|n| n as i64)
        })
    });
    match result {
        Ok(n) => encode_success_string(&n.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_recv_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid receive length");
    }
    let result = udp_handle(handle).and_then(|socket| {
        let mut buf = vec![0u8; count as usize];
        let n = crate::gc::native_wait(|| {
            socket
                .lock()
                .expect("network UDP socket poisoned")
                .recv(&mut buf)
        })?;
        buf.truncate(n);
        Ok(buf)
    });
    match result {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_peek_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid peek length");
    }
    let result = udp_handle(handle).and_then(|socket| {
        let mut buf = vec![0u8; count as usize];
        let n = crate::gc::native_wait(|| {
            socket
                .lock()
                .expect("network UDP socket poisoned")
                .peek(&mut buf)
        })?;
        buf.truncate(n);
        Ok(buf)
    });
    match result {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_send_to_encoded(handle: i64, contents_hex: String, addr: String) -> String {
    let result = decode_hex_bytes(&contents_hex).and_then(|bytes| {
        let addr = addr.parse::<SocketAddr>().map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
        })?;
        udp_handle(handle).and_then(|socket| {
            crate::gc::native_wait(|| {
                socket
                    .lock()
                    .expect("network UDP socket poisoned")
                    .send_to(&bytes, addr)
            })
            .map(|n| n as i64)
        })
    });
    match result {
        Ok(n) => encode_success_string(&n.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_recv_from_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid receive length");
    }
    let result = udp_handle(handle).and_then(|socket| {
        let mut buf = vec![0u8; count as usize];
        let (n, source) = crate::gc::native_wait(|| {
            socket
                .lock()
                .expect("network UDP socket poisoned")
                .recv_from(&mut buf)
        })?;
        buf.truncate(n);
        Ok((buf, source.to_string()))
    });
    match result {
        Ok((bytes, source)) => {
            let mut payload = String::new();
            push_field(&mut payload, &bytes_hex_payload(&bytes));
            push_field(&mut payload, &source);
            encode_success_string(&payload)
        }
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn net_udp_peek_from_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid peek length");
    }
    let result = udp_handle(handle).and_then(|socket| {
        let mut buf = vec![0u8; count as usize];
        let (n, source) = crate::gc::native_wait(|| {
            socket
                .lock()
                .expect("network UDP socket poisoned")
                .peek_from(&mut buf)
        })?;
        buf.truncate(n);
        Ok((buf, source.to_string()))
    });
    match result {
        Ok((bytes, source)) => {
            let mut payload = String::new();
            push_field(&mut payload, &bytes_hex_payload(&bytes));
            push_field(&mut payload, &source);
            encode_success_string(&payload)
        }
        Err(err) => encode_error_string(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn resolves_ipv4_literal_to_length_prefixed_payload() {
        let result = net_resolve_encoded("127.0.0.1".to_string());
        assert_eq!(result, "09:127.0.0.1");
    }

    #[test]
    fn invalid_host_reports_error_payload() {
        let result = net_resolve_encoded(String::new());
        assert!(result.starts_with('1'), "{result}");
        assert!(result.len() > 1, "{result}");
    }

    #[test]
    fn tcp_listener_bind_uses_native_state_helper() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = bind_tcp_listener_native_wait(addr).expect("ephemeral TCP listener bind");
        let handle = register_listener(listener);
        assert_eq!(net_tcp_listener_close_encoded(handle), "0");
    }

    #[test]
    fn udp_socket_bind_uses_native_state_helper() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let socket = bind_udp_socket_native_wait(addr).expect("ephemeral UDP socket bind");
        let handle = register_udp_socket(socket);
        assert_eq!(net_udp_close_encoded(handle), "0");
    }

    #[test]
    fn tcp_stream_control_hooks_use_native_state_helper() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = thread::spawn(move || {
            let _accepted = listener.accept().unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        let handle = register_stream(stream);
        assert!(net_tcp_stream_peer_addr_encoded(handle).starts_with('0'));
        assert!(net_tcp_stream_local_addr_encoded(handle).starts_with('0'));
        assert_eq!(net_tcp_stream_take_error_encoded(handle), "00");
        assert_eq!(net_tcp_stream_set_nodelay_encoded(handle, 1), "0");
        assert_eq!(net_tcp_stream_nodelay_encoded(handle), "01");
        assert_eq!(net_tcp_stream_set_nonblocking_encoded(handle, 1), "0");
        assert_eq!(net_tcp_stream_set_nonblocking_encoded(handle, 0), "0");
        assert_eq!(
            net_tcp_stream_set_read_timeout_encoded(handle, 1_000_000, 1),
            "0"
        );
        assert_eq!(net_tcp_stream_read_timeout_encoded(handle), "01000000");
        assert_eq!(
            net_tcp_stream_set_write_timeout_encoded(handle, 1_000_000, 1),
            "0"
        );
        assert_eq!(net_tcp_stream_write_timeout_encoded(handle), "01000000");
        assert_eq!(net_tcp_stream_set_ttl_encoded(handle, 64), "0");
        assert_eq!(net_tcp_stream_ttl_encoded(handle), "064");
        assert_eq!(net_tcp_stream_close_encoded(handle), "0");
        accept.join().unwrap();
    }

    #[test]
    fn tcp_listener_control_hooks_use_native_state_helper() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let handle = register_listener(listener);

        assert!(net_tcp_listener_local_addr_encoded(handle).starts_with('0'));
        assert_eq!(net_tcp_listener_take_error_encoded(handle), "00");
        assert_eq!(net_tcp_listener_set_nonblocking_encoded(handle, 1), "0");
        assert_eq!(net_tcp_listener_set_nonblocking_encoded(handle, 0), "0");
        assert_eq!(net_tcp_listener_set_ttl_encoded(handle, 64), "0");
        assert_eq!(net_tcp_listener_ttl_encoded(handle), "064");
        assert_eq!(net_tcp_listener_close_encoded(handle), "0");
    }

    #[test]
    fn udp_control_hooks_use_native_state_helper() {
        let peer = OsUdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = peer.local_addr().unwrap().to_string();
        let socket = OsUdpSocket::bind("127.0.0.1:0").unwrap();
        let handle = register_udp_socket(socket);

        assert!(net_udp_local_addr_encoded(handle).starts_with('0'));
        assert_eq!(net_udp_take_error_encoded(handle), "00");
        assert_eq!(net_udp_connect_encoded(handle, peer_addr.clone()), "0");
        assert_eq!(net_udp_peer_addr_encoded(handle), format!("0{peer_addr}"));
        assert_eq!(net_udp_set_nonblocking_encoded(handle, 1), "0");
        assert_eq!(net_udp_set_nonblocking_encoded(handle, 0), "0");
        assert_eq!(net_udp_set_read_timeout_encoded(handle, 1_000_000, 1), "0");
        assert_eq!(net_udp_read_timeout_encoded(handle), "01000000");
        assert_eq!(net_udp_set_read_timeout_encoded(handle, 0, 0), "0");
        assert_eq!(net_udp_read_timeout_encoded(handle), "0n");
        assert_eq!(net_udp_set_write_timeout_encoded(handle, 1_000_000, 1), "0");
        assert_eq!(net_udp_write_timeout_encoded(handle), "01000000");
        assert_eq!(net_udp_set_write_timeout_encoded(handle, 0, 0), "0");
        assert_eq!(net_udp_write_timeout_encoded(handle), "0n");
        assert!(
            net_udp_set_read_timeout_encoded(handle, -1, 1).starts_with("1invalid socket timeout")
        );
        assert_eq!(net_udp_set_ttl_encoded(handle, 64), "0");
        assert_eq!(net_udp_ttl_encoded(handle), "064");
        assert_eq!(net_udp_set_broadcast_encoded(handle, 1), "0");
        assert_eq!(net_udp_broadcast_encoded(handle), "01");
        assert_eq!(net_udp_set_broadcast_encoded(handle, 0), "0");
        assert_eq!(net_udp_broadcast_encoded(handle), "00");
        assert_eq!(net_udp_set_multicast_loop_v4_encoded(handle, 1), "0");
        assert_eq!(net_udp_multicast_loop_v4_encoded(handle), "01");
        assert_eq!(net_udp_set_multicast_loop_v4_encoded(handle, 0), "0");
        assert_eq!(net_udp_multicast_loop_v4_encoded(handle), "00");
        assert_eq!(net_udp_set_multicast_ttl_v4_encoded(handle, 32), "0");
        assert_eq!(net_udp_multicast_ttl_v4_encoded(handle), "032");
        assert_eq!(net_udp_close_encoded(handle), "0");
    }

    #[test]
    fn tcp_stream_close_drops_removed_handle_inside_native_state_marker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accept = thread::spawn(move || {
            let _accepted = listener.accept().unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        let handle = register_stream(stream);
        assert_eq!(net_tcp_stream_close_encoded(handle), "0");
        assert!(
            net_tcp_stream_close_encoded(handle).starts_with('1'),
            "closing an already removed TCP stream should report an error"
        );
        accept.join().unwrap();
    }

    #[test]
    fn tcp_listener_close_drops_removed_handle_inside_native_state_marker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let handle = register_listener(listener);
        assert_eq!(net_tcp_listener_close_encoded(handle), "0");
        assert!(
            net_tcp_listener_close_encoded(handle).starts_with('1'),
            "closing an already removed TCP listener should report an error"
        );
    }

    #[test]
    fn udp_close_drops_removed_handle_inside_native_state_marker() {
        let socket = OsUdpSocket::bind("127.0.0.1:0").unwrap();
        let handle = register_udp_socket(socket);
        assert_eq!(net_udp_close_encoded(handle), "0");
        assert!(
            net_udp_close_encoded(handle).starts_with('1'),
            "closing an already removed UDP socket should report an error"
        );
    }

    #[test]
    fn tcp_listener_accept_and_stream_round_trip_use_length_framed_payloads() {
        let listener = net_tcp_listener_bind_encoded("127.0.0.1:0".to_string());
        assert!(listener.starts_with('0'), "{listener}");
        let listener_handle: i64 = listener[1..].parse().unwrap();

        let local = net_tcp_listener_local_addr_encoded(listener_handle);
        assert!(local.starts_with('0'), "{local}");
        let local_addr = local[1..].to_string();
        assert_eq!(net_tcp_listener_take_error_encoded(listener_handle), "00");
        assert_eq!(net_tcp_listener_set_ttl_encoded(listener_handle, 64), "0");
        assert_eq!(net_tcp_listener_ttl_encoded(listener_handle), "064");
        assert_eq!(
            net_tcp_listener_set_nonblocking_encoded(listener_handle, 1),
            "0"
        );
        assert_eq!(
            net_tcp_listener_set_nonblocking_encoded(listener_handle, 0),
            "0"
        );

        let server = thread::spawn(move || {
            let accepted = net_tcp_listener_accept_encoded(listener_handle);
            assert!(accepted.starts_with('0'), "{accepted}");
            let payload = &accepted[1..];
            let colon = payload.find(':').unwrap();
            let handle_len: usize = payload[..colon].parse().unwrap();
            let handle_start = colon + 1;
            let handle_end = handle_start + handle_len;
            let stream_handle: i64 = payload[handle_start..handle_end].parse().unwrap();

            assert_eq!(net_tcp_stream_take_error_encoded(stream_handle), "00");
            assert_eq!(
                net_tcp_stream_set_nonblocking_encoded(stream_handle, 1),
                "0"
            );
            assert_eq!(
                net_tcp_stream_set_nonblocking_encoded(stream_handle, 0),
                "0"
            );
            assert_eq!(
                net_tcp_stream_set_read_timeout_encoded(stream_handle, 1_000_000, 1),
                "0"
            );
            assert_eq!(
                net_tcp_stream_read_timeout_encoded(stream_handle),
                "01000000"
            );
            assert_eq!(
                net_tcp_stream_set_read_timeout_encoded(stream_handle, 0, 0),
                "0"
            );
            assert_eq!(net_tcp_stream_read_timeout_encoded(stream_handle), "0n");
            assert_eq!(
                net_tcp_stream_set_write_timeout_encoded(stream_handle, 1_000_000, 1),
                "0"
            );
            assert_eq!(
                net_tcp_stream_write_timeout_encoded(stream_handle),
                "01000000"
            );
            assert_eq!(
                net_tcp_stream_set_write_timeout_encoded(stream_handle, 0, 0),
                "0"
            );
            assert_eq!(net_tcp_stream_write_timeout_encoded(stream_handle), "0n");
            assert!(
                net_tcp_stream_set_read_timeout_encoded(stream_handle, -1, 1)
                    .starts_with("1invalid socket timeout")
            );
            assert_eq!(net_tcp_stream_peek_encoded(stream_handle, 4), "070696e67");
            assert!(
                net_tcp_stream_peek_encoded(stream_handle, -1).starts_with("1invalid peek length")
            );
            let read = net_tcp_stream_read_encoded(stream_handle, 4);
            assert_eq!(read, "070696e67");
            assert_eq!(net_tcp_stream_set_nodelay_encoded(stream_handle, 1), "0");
            assert_eq!(net_tcp_stream_nodelay_encoded(stream_handle), "01");
            assert_eq!(net_tcp_stream_set_ttl_encoded(stream_handle, 64), "0");
            assert_eq!(net_tcp_stream_ttl_encoded(stream_handle), "064");
            let write = net_tcp_stream_write_all_encoded(stream_handle, "706f6e67".to_string());
            assert_eq!(write, "0");
            assert_eq!(net_tcp_stream_close_encoded(stream_handle), "0");
        });

        let client = net_tcp_connect_encoded(local_addr.clone());
        assert!(client.starts_with('0'), "{client}");
        let client_handle: i64 = client[1..].parse().unwrap();
        let local = net_tcp_stream_local_addr_encoded(client_handle);
        assert!(local.starts_with('0'), "{local}");
        let peer = net_tcp_stream_peer_addr_encoded(client_handle);
        assert_eq!(peer, format!("0{local_addr}"));
        assert_eq!(net_tcp_stream_take_error_encoded(client_handle), "00");
        assert_eq!(net_tcp_stream_set_nodelay_encoded(client_handle, 1), "0");
        assert_eq!(net_tcp_stream_nodelay_encoded(client_handle), "01");
        assert_eq!(net_tcp_stream_set_ttl_encoded(client_handle, 64), "0");
        assert_eq!(net_tcp_stream_ttl_encoded(client_handle), "064");
        assert_eq!(
            net_tcp_stream_set_nonblocking_encoded(client_handle, 1),
            "0"
        );
        assert_eq!(
            net_tcp_stream_set_nonblocking_encoded(client_handle, 0),
            "0"
        );
        assert_eq!(
            net_tcp_stream_set_read_timeout_encoded(client_handle, 1_000_000, 1),
            "0"
        );
        assert_eq!(
            net_tcp_stream_read_timeout_encoded(client_handle),
            "01000000"
        );
        assert_eq!(
            net_tcp_stream_set_read_timeout_encoded(client_handle, 0, 0),
            "0"
        );
        assert_eq!(net_tcp_stream_read_timeout_encoded(client_handle), "0n");
        assert_eq!(
            net_tcp_stream_set_write_timeout_encoded(client_handle, 1_000_000, 1),
            "0"
        );
        assert_eq!(
            net_tcp_stream_write_timeout_encoded(client_handle),
            "01000000"
        );
        assert_eq!(
            net_tcp_stream_set_write_timeout_encoded(client_handle, 0, 0),
            "0"
        );
        assert_eq!(net_tcp_stream_write_timeout_encoded(client_handle), "0n");
        assert_eq!(
            net_tcp_stream_write_all_encoded(client_handle, "70696e67".to_string()),
            "0"
        );
        let read = net_tcp_stream_read_encoded(client_handle, 4);
        assert_eq!(read, "0706f6e67");
        assert_eq!(net_tcp_stream_close_encoded(client_handle), "0");
        server.join().unwrap();

        let listener = net_tcp_listener_bind_encoded("127.0.0.1:0".to_string());
        assert!(listener.starts_with('0'), "{listener}");
        let listener_handle: i64 = listener[1..].parse().unwrap();
        let local = net_tcp_listener_local_addr_encoded(listener_handle);
        assert!(local.starts_with('0'), "{local}");
        let local_addr = local[1..].to_string();
        let timed_server = thread::spawn(move || {
            let accepted = net_tcp_listener_accept_encoded(listener_handle);
            assert!(accepted.starts_with('0'), "{accepted}");
            let payload = &accepted[1..];
            let colon = payload.find(':').unwrap();
            let handle_len: usize = payload[..colon].parse().unwrap();
            let handle_start = colon + 1;
            let handle_end = handle_start + handle_len;
            let stream_handle: i64 = payload[handle_start..handle_end].parse().unwrap();
            assert_eq!(net_tcp_stream_close_encoded(stream_handle), "0");
            assert_eq!(net_tcp_listener_close_encoded(listener_handle), "0");
        });
        let timed_client = net_tcp_connect_timeout_encoded(local_addr.clone(), 1_000_000_000);
        assert!(timed_client.starts_with('0'), "{timed_client}");
        let timed_client_handle: i64 = timed_client[1..].parse().unwrap();
        assert_eq!(net_tcp_stream_close_encoded(timed_client_handle), "0");
        timed_server.join().unwrap();
        let invalid_timeout = net_tcp_connect_timeout_encoded(local_addr.clone(), -1);
        assert!(
            invalid_timeout.starts_with("1invalid socket timeout"),
            "{invalid_timeout}"
        );
    }

    #[test]
    fn udp_sockets_send_to_and_recv_from_use_length_framed_payloads() {
        let receiver = net_udp_bind_encoded("127.0.0.1:0".to_string());
        assert!(receiver.starts_with('0'), "{receiver}");
        let receiver_handle: i64 = receiver[1..].parse().unwrap();
        let receiver_addr = net_udp_local_addr_encoded(receiver_handle);
        assert!(receiver_addr.starts_with('0'), "{receiver_addr}");
        let receiver_addr = receiver_addr[1..].to_string();

        let sender = net_udp_bind_encoded("127.0.0.1:0".to_string());
        assert!(sender.starts_with('0'), "{sender}");
        let sender_handle: i64 = sender[1..].parse().unwrap();
        let sender_addr = net_udp_local_addr_encoded(sender_handle);
        assert!(sender_addr.starts_with('0'), "{sender_addr}");
        let sender_addr = sender_addr[1..].to_string();
        assert_eq!(net_udp_take_error_encoded(sender_handle), "00");
        assert_eq!(net_udp_set_nonblocking_encoded(sender_handle, 1), "0");
        assert_eq!(net_udp_set_nonblocking_encoded(sender_handle, 0), "0");
        assert_eq!(
            net_udp_set_read_timeout_encoded(sender_handle, 1_000_000, 1),
            "0"
        );
        assert_eq!(net_udp_read_timeout_encoded(sender_handle), "01000000");
        assert_eq!(net_udp_set_read_timeout_encoded(sender_handle, 0, 0), "0");
        assert_eq!(net_udp_read_timeout_encoded(sender_handle), "0n");
        assert_eq!(
            net_udp_set_write_timeout_encoded(sender_handle, 1_000_000, 1),
            "0"
        );
        assert_eq!(net_udp_write_timeout_encoded(sender_handle), "01000000");
        assert_eq!(net_udp_set_write_timeout_encoded(sender_handle, 0, 0), "0");
        assert_eq!(net_udp_write_timeout_encoded(sender_handle), "0n");
        assert!(
            net_udp_set_read_timeout_encoded(sender_handle, -1, 1)
                .starts_with("1invalid socket timeout")
        );
        assert_eq!(net_udp_set_ttl_encoded(sender_handle, 64), "0");
        assert_eq!(net_udp_ttl_encoded(sender_handle), "064");
        assert_eq!(net_udp_set_broadcast_encoded(sender_handle, 1), "0");
        assert_eq!(net_udp_broadcast_encoded(sender_handle), "01");
        assert_eq!(net_udp_set_broadcast_encoded(sender_handle, 0), "0");
        assert_eq!(net_udp_broadcast_encoded(sender_handle), "00");
        assert_eq!(net_udp_set_multicast_loop_v4_encoded(sender_handle, 1), "0");
        assert_eq!(net_udp_multicast_loop_v4_encoded(sender_handle), "01");
        assert_eq!(net_udp_set_multicast_loop_v4_encoded(sender_handle, 0), "0");
        assert_eq!(net_udp_multicast_loop_v4_encoded(sender_handle), "00");
        assert_eq!(net_udp_set_multicast_ttl_v4_encoded(sender_handle, 32), "0");
        assert_eq!(net_udp_multicast_ttl_v4_encoded(sender_handle), "032");
        let group_v4 = "224.0.0.251".to_string();
        let iface_v4 = "0.0.0.0".to_string();
        assert_eq!(
            net_udp_join_multicast_v4_encoded(sender_handle, group_v4.clone(), iface_v4.clone()),
            "0"
        );
        assert_eq!(
            net_udp_leave_multicast_v4_encoded(sender_handle, group_v4.clone(), iface_v4),
            "0"
        );

        let sender_v6 = net_udp_bind_encoded("[::1]:0".to_string());
        assert!(sender_v6.starts_with('0'), "{sender_v6}");
        let sender_v6_handle: i64 = sender_v6[1..].parse().unwrap();
        assert_eq!(
            net_udp_set_multicast_loop_v6_encoded(sender_v6_handle, 1),
            "0"
        );
        assert_eq!(net_udp_multicast_loop_v6_encoded(sender_v6_handle), "01");
        assert_eq!(
            net_udp_set_multicast_loop_v6_encoded(sender_v6_handle, 0),
            "0"
        );
        assert_eq!(net_udp_multicast_loop_v6_encoded(sender_v6_handle), "00");
        let group_v6 = "ff02::1".to_string();
        let joined_v6 = net_udp_join_multicast_v6_encoded(sender_v6_handle, group_v6.clone(), 0);
        assert!(
            joined_v6 == "0" || joined_v6.starts_with('1'),
            "{joined_v6}"
        );
        let left_v6 = net_udp_leave_multicast_v6_encoded(sender_v6_handle, group_v6.clone(), 0);
        assert!(left_v6 == "0" || left_v6.starts_with('1'), "{left_v6}");
        let invalid_group_v6 = net_udp_join_multicast_v6_encoded(sender_v6_handle, group_v4, 0);
        assert!(invalid_group_v6.starts_with('1'), "{invalid_group_v6}");
        assert!(
            invalid_group_v6.contains("invalid IPv6 multicast group"),
            "{invalid_group_v6}"
        );
        let invalid_iface_v6 = net_udp_join_multicast_v6_encoded(sender_v6_handle, group_v6, -1);
        assert_eq!(invalid_iface_v6, "1invalid IPv6 multicast interface index");
        assert_eq!(net_udp_close_encoded(sender_v6_handle), "0");

        let sent = net_udp_send_to_encoded(
            sender_handle,
            "68656c6c6f".to_string(),
            receiver_addr.clone(),
        );
        assert_eq!(sent, "05");

        let peeked = net_udp_peek_from_encoded(receiver_handle, 16);
        assert!(peeked.starts_with('0'), "{peeked}");
        assert!(net_udp_peek_from_encoded(receiver_handle, -1).starts_with("1invalid peek length"));
        let received = net_udp_recv_from_encoded(receiver_handle, 16);
        assert_eq!(peeked, received);
        assert!(received.starts_with('0'), "{received}");
        let payload = &received[1..];
        let first_colon = payload.find(':').unwrap();
        let hex_len: usize = payload[..first_colon].parse().unwrap();
        let hex_start = first_colon + 1;
        let hex_end = hex_start + hex_len;
        assert_eq!(&payload[hex_start..hex_end], "68656c6c6f");
        let rest = &payload[hex_end..];
        let second_colon = rest.find(':').unwrap();
        let addr_len: usize = rest[..second_colon].parse().unwrap();
        let addr_start = second_colon + 1;
        let addr_end = addr_start + addr_len;
        assert_eq!(&rest[addr_start..addr_end], sender_addr);

        assert_eq!(
            net_udp_connect_encoded(sender_handle, receiver_addr.clone()),
            "0"
        );
        assert_eq!(
            net_udp_peer_addr_encoded(sender_handle),
            format!("0{receiver_addr}")
        );
        assert_eq!(
            net_udp_connect_encoded(receiver_handle, sender_addr.clone()),
            "0"
        );
        assert_eq!(
            net_udp_peer_addr_encoded(receiver_handle),
            format!("0{sender_addr}")
        );
        assert_eq!(
            net_udp_send_encoded(sender_handle, "70696e67".to_string()),
            "04"
        );
        assert_eq!(net_udp_peek_encoded(receiver_handle, 16), "070696e67");
        assert!(net_udp_peek_encoded(receiver_handle, -1).starts_with("1invalid peek length"));
        assert_eq!(net_udp_recv_encoded(receiver_handle, 16), "070696e67");

        assert_eq!(net_udp_close_encoded(sender_handle), "0");
        assert_eq!(net_udp_close_encoded(receiver_handle), "0");
    }
}
