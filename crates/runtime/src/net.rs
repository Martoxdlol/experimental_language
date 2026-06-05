//! Runtime networking hooks for `std:net`.
//!
//! The public network value contracts live in Otter Fusion source. Runtime
//! hooks only expose provider-backed host operations using compact string
//! payloads decoded by the stdlib layer.

use crate::strings::{LangStr, lang_str_from_utf8, str_bytes};
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket as OsUdpSocket};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

fn push_field(out: &mut String, value: &str) {
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

unsafe fn read_lang_str(s: *const LangStr) -> String {
    String::from_utf8_lossy(unsafe { str_bytes(s) }).into_owned()
}

fn make_lang_str(s: &str) -> *const LangStr {
    unsafe { lang_str_from_utf8(s.as_ptr(), s.len()) }
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

fn encode_success(payload: &str) -> *const LangStr {
    make_lang_str(&encode_success_string(payload))
}

fn encode_error(error: impl std::fmt::Display) -> *const LangStr {
    make_lang_str(&encode_error_string(error))
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

fn register_listener(listener: TcpListener) -> i64 {
    let handle = NEXT_LISTENER_HANDLE.fetch_add(1, Ordering::Relaxed);
    listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .insert(handle, Arc::new(Mutex::new(listener)));
    handle
}

fn register_udp_socket(socket: OsUdpSocket) -> i64 {
    let handle = NEXT_UDP_HANDLE.fetch_add(1, Ordering::Relaxed);
    udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .insert(handle, Arc::new(Mutex::new(socket)));
    handle
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

/// Resolve `host` through the selected provider's address resolver.
///
/// The returned payload is tagged: `0` followed by length-prefixed textual IP
/// fields on success, or `1<message>` on provider error.
///
/// # Safety
/// `host` must be a valid language string pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_resolve(host: *const LangStr) -> *const LangStr {
    let host = unsafe { read_lang_str(host) };
    let payload = match resolve_host(&host) {
        Ok(addrs) => {
            let mut out = String::from("0");
            for addr in addrs {
                push_field(&mut out, &addr);
            }
            out
        }
        Err(err) => format!("1{err}"),
    };
    make_lang_str(&payload)
}

/// Connect a TCP stream to a textual socket address.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_connect(addr: *const LangStr) -> *const LangStr {
    let addr = unsafe { read_lang_str(addr) };
    match addr
        .parse::<SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|addr| TcpStream::connect(addr).map_err(|err| err.to_string()))
    {
        Ok(stream) => encode_success(&register_stream(stream).to_string()),
        Err(err) => encode_error(err),
    }
}

/// Release a TCP stream handle from the runtime registry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_release(handle: i64) {
    stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .remove(&handle);
}

/// Close a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_close(handle: i64) -> *const LangStr {
    let removed = stream_registry()
        .lock()
        .expect("network stream registry poisoned")
        .remove(&handle);
    match removed {
        Some(_) => encode_success(""),
        None => encode_error("invalid TCP stream handle"),
    }
}

/// Return the peer address of a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_peer_addr(handle: i64) -> *const LangStr {
    match stream_handle(handle).and_then(|stream| {
        stream
            .lock()
            .expect("network stream poisoned")
            .peer_addr()
            .map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success(&addr),
        Err(err) => encode_error(err),
    }
}

/// Return the local address of a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_local_addr(handle: i64) -> *const LangStr {
    match stream_handle(handle).and_then(|stream| {
        stream
            .lock()
            .expect("network stream poisoned")
            .local_addr()
            .map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success(&addr),
        Err(err) => encode_error(err),
    }
}

/// Enable or disable `TCP_NODELAY` on a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_set_nodelay(handle: i64, on: i64) -> *const LangStr {
    match stream_handle(handle).and_then(|stream| {
        stream
            .lock()
            .expect("network stream poisoned")
            .set_nodelay(on != 0)
    }) {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

/// Read up to `count` bytes from a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_read(handle: i64, count: i64) -> *const LangStr {
    if count < 0 {
        return encode_error("invalid read length");
    }
    match stream_handle(handle).and_then(|stream| {
        let mut buf = vec![0u8; count as usize];
        let n = stream
            .lock()
            .expect("network stream poisoned")
            .read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }) {
        Ok(bytes) => encode_success(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error(err),
    }
}

/// Read bytes from a TCP stream handle until EOF.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_read_to_end(handle: i64) -> *const LangStr {
    match stream_handle(handle).and_then(|stream| {
        let mut buf = Vec::new();
        stream
            .lock()
            .expect("network stream poisoned")
            .read_to_end(&mut buf)?;
        Ok(buf)
    }) {
        Ok(bytes) => encode_success(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error(err),
    }
}

/// Write bytes to a TCP stream handle.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_stream_write(
    handle: i64,
    contents_hex: *const LangStr,
) -> *const LangStr {
    let contents_hex = unsafe { read_lang_str(contents_hex) };
    match decode_hex_bytes(&contents_hex).and_then(|bytes| {
        stream_handle(handle).and_then(|stream| {
            stream
                .lock()
                .expect("network stream poisoned")
                .write(&bytes)
                .map(|n| n as i64)
        })
    }) {
        Ok(n) => encode_success(&n.to_string()),
        Err(err) => encode_error(err),
    }
}

/// Write all bytes to a TCP stream handle.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_stream_write_all(
    handle: i64,
    contents_hex: *const LangStr,
) -> *const LangStr {
    let contents_hex = unsafe { read_lang_str(contents_hex) };
    match decode_hex_bytes(&contents_hex).and_then(|bytes| {
        stream_handle(handle).and_then(|stream| {
            stream
                .lock()
                .expect("network stream poisoned")
                .write_all(&bytes)
        })
    }) {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

/// Flush a TCP stream handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_stream_flush(handle: i64) -> *const LangStr {
    match stream_handle(handle)
        .and_then(|stream| stream.lock().expect("network stream poisoned").flush())
    {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

/// Bind a TCP listener to a textual socket address.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_tcp_listener_bind(addr: *const LangStr) -> *const LangStr {
    let addr = unsafe { read_lang_str(addr) };
    match addr
        .parse::<SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|addr| TcpListener::bind(addr).map_err(|err| err.to_string()))
    {
        Ok(listener) => encode_success(&register_listener(listener).to_string()),
        Err(err) => encode_error(err),
    }
}

/// Release a TCP listener handle from the runtime registry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_release(handle: i64) {
    listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .remove(&handle);
}

/// Close a TCP listener handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_close(handle: i64) -> *const LangStr {
    let removed = listener_registry()
        .lock()
        .expect("network listener registry poisoned")
        .remove(&handle);
    match removed {
        Some(_) => encode_success(""),
        None => encode_error("invalid TCP listener handle"),
    }
}

/// Accept one connection from a TCP listener handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_accept(handle: i64) -> *const LangStr {
    match listener_handle(handle).and_then(|listener| {
        let (stream, peer) = listener
            .lock()
            .expect("network listener poisoned")
            .accept()?;
        Ok((register_stream(stream), peer.to_string()))
    }) {
        Ok((stream_handle, peer)) => {
            let mut payload = String::new();
            push_field(&mut payload, &stream_handle.to_string());
            push_field(&mut payload, &peer);
            encode_success(&payload)
        }
        Err(err) => encode_error(err),
    }
}

/// Return the local address of a TCP listener handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_tcp_listener_local_addr(handle: i64) -> *const LangStr {
    match listener_handle(handle).and_then(|listener| {
        listener
            .lock()
            .expect("network listener poisoned")
            .local_addr()
            .map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success(&addr),
        Err(err) => encode_error(err),
    }
}

/// Bind a UDP socket to a textual socket address.
///
/// # Safety
/// `addr` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_bind(addr: *const LangStr) -> *const LangStr {
    let addr = unsafe { read_lang_str(addr) };
    match addr
        .parse::<SocketAddr>()
        .map_err(|err| err.to_string())
        .and_then(|addr| OsUdpSocket::bind(addr).map_err(|err| err.to_string()))
    {
        Ok(socket) => encode_success(&register_udp_socket(socket).to_string()),
        Err(err) => encode_error(err),
    }
}

/// Release a UDP socket handle from the runtime registry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_release(handle: i64) {
    udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .remove(&handle);
}

/// Close a UDP socket handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_close(handle: i64) -> *const LangStr {
    let removed = udp_registry()
        .lock()
        .expect("network UDP registry poisoned")
        .remove(&handle);
    match removed {
        Some(_) => encode_success(""),
        None => encode_error("invalid UDP socket handle"),
    }
}

/// Return the local address of a UDP socket handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_local_addr(handle: i64) -> *const LangStr {
    match udp_handle(handle).and_then(|socket| {
        socket
            .lock()
            .expect("network UDP socket poisoned")
            .local_addr()
            .map(|addr| addr.to_string())
    }) {
        Ok(addr) => encode_success(&addr),
        Err(err) => encode_error(err),
    }
}

/// Send bytes from a UDP socket handle to a textual socket address.
///
/// # Safety
/// `contents_hex` and `addr` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_net_udp_send_to(
    handle: i64,
    contents_hex: *const LangStr,
    addr: *const LangStr,
) -> *const LangStr {
    let contents_hex = unsafe { read_lang_str(contents_hex) };
    let addr = unsafe { read_lang_str(addr) };
    match decode_hex_bytes(&contents_hex).and_then(|bytes| {
        let addr = addr.parse::<SocketAddr>().map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, err.to_string())
        })?;
        udp_handle(handle).and_then(|socket| {
            socket
                .lock()
                .expect("network UDP socket poisoned")
                .send_to(&bytes, addr)
                .map(|n| n as i64)
        })
    }) {
        Ok(n) => encode_success(&n.to_string()),
        Err(err) => encode_error(err),
    }
}

/// Receive up to `count` bytes from a UDP socket handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_net_udp_recv_from(handle: i64, count: i64) -> *const LangStr {
    if count < 0 {
        return encode_error("invalid receive length");
    }
    match udp_handle(handle).and_then(|socket| {
        let mut buf = vec![0u8; count as usize];
        let (n, source) = socket
            .lock()
            .expect("network UDP socket poisoned")
            .recv_from(&mut buf)?;
        buf.truncate(n);
        Ok((buf, source.to_string()))
    }) {
        Ok((bytes, source)) => {
            let mut payload = String::new();
            push_field(&mut payload, &bytes_hex_payload(&bytes));
            push_field(&mut payload, &source);
            encode_success(&payload)
        }
        Err(err) => encode_error(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn decode(ptr: *const LangStr) -> String {
        String::from_utf8_lossy(unsafe { str_bytes(ptr) }).into_owned()
    }

    #[test]
    fn resolves_ipv4_literal_to_length_prefixed_payload() {
        let result = unsafe { decode(lang_net_resolve(make_lang_str("127.0.0.1"))) };
        assert_eq!(result, "09:127.0.0.1");
    }

    #[test]
    fn invalid_host_reports_error_payload() {
        let result = unsafe { decode(lang_net_resolve(make_lang_str(""))) };
        assert!(result.starts_with('1'), "{result}");
        assert!(result.len() > 1, "{result}");
    }

    #[test]
    fn tcp_listener_accept_and_stream_round_trip_use_length_framed_payloads() {
        let listener = unsafe { decode(lang_net_tcp_listener_bind(make_lang_str("127.0.0.1:0"))) };
        assert!(listener.starts_with('0'), "{listener}");
        let listener_handle: i64 = listener[1..].parse().unwrap();

        let local = decode(lang_net_tcp_listener_local_addr(listener_handle));
        assert!(local.starts_with('0'), "{local}");
        let local_addr = local[1..].to_string();

        let server = thread::spawn(move || {
            let accepted = decode(lang_net_tcp_listener_accept(listener_handle));
            assert!(accepted.starts_with('0'), "{accepted}");
            let payload = &accepted[1..];
            let colon = payload.find(':').unwrap();
            let handle_len: usize = payload[..colon].parse().unwrap();
            let handle_start = colon + 1;
            let handle_end = handle_start + handle_len;
            let stream_handle: i64 = payload[handle_start..handle_end].parse().unwrap();

            let read = decode(lang_net_tcp_stream_read(stream_handle, 4));
            assert_eq!(read, "070696e67");
            let write = unsafe {
                decode(lang_net_tcp_stream_write_all(
                    stream_handle,
                    make_lang_str("706f6e67"),
                ))
            };
            assert_eq!(write, "0");
            assert_eq!(decode(lang_net_tcp_stream_close(stream_handle)), "0");
        });

        let client = unsafe { decode(lang_net_tcp_connect(make_lang_str(&local_addr))) };
        assert!(client.starts_with('0'), "{client}");
        let client_handle: i64 = client[1..].parse().unwrap();
        let local = decode(lang_net_tcp_stream_local_addr(client_handle));
        assert!(local.starts_with('0'), "{local}");
        let peer = decode(lang_net_tcp_stream_peer_addr(client_handle));
        assert_eq!(peer, format!("0{local_addr}"));
        assert_eq!(
            unsafe {
                decode(lang_net_tcp_stream_write_all(
                    client_handle,
                    make_lang_str("70696e67"),
                ))
            },
            "0"
        );
        let read = decode(lang_net_tcp_stream_read(client_handle, 4));
        assert_eq!(read, "0706f6e67");
        assert_eq!(decode(lang_net_tcp_stream_close(client_handle)), "0");
        server.join().unwrap();
    }

    #[test]
    fn udp_sockets_send_to_and_recv_from_use_length_framed_payloads() {
        let receiver = unsafe { decode(lang_net_udp_bind(make_lang_str("127.0.0.1:0"))) };
        assert!(receiver.starts_with('0'), "{receiver}");
        let receiver_handle: i64 = receiver[1..].parse().unwrap();
        let receiver_addr = decode(lang_net_udp_local_addr(receiver_handle));
        assert!(receiver_addr.starts_with('0'), "{receiver_addr}");
        let receiver_addr = receiver_addr[1..].to_string();

        let sender = unsafe { decode(lang_net_udp_bind(make_lang_str("127.0.0.1:0"))) };
        assert!(sender.starts_with('0'), "{sender}");
        let sender_handle: i64 = sender[1..].parse().unwrap();
        let sender_addr = decode(lang_net_udp_local_addr(sender_handle));
        assert!(sender_addr.starts_with('0'), "{sender_addr}");
        let sender_addr = sender_addr[1..].to_string();

        let sent = unsafe {
            decode(lang_net_udp_send_to(
                sender_handle,
                make_lang_str("68656c6c6f"),
                make_lang_str(&receiver_addr),
            ))
        };
        assert_eq!(sent, "05");

        let received = decode(lang_net_udp_recv_from(receiver_handle, 16));
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

        assert_eq!(decode(lang_net_udp_close(sender_handle)), "0");
        assert_eq!(decode(lang_net_udp_close(receiver_handle)), "0");
    }
}
