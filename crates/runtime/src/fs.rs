//! Runtime filesystem hooks for the target-backed `std:fs` slices.
//!
//! The public API is authored in `stdlib_src/std/fs.otter`. These hooks expose a
//! small filesystem boundary over the stable runtime `str` ABI. Results are
//! encoded as a private tagged string: `"0" + payload` for success and
//! `"1" + message` for an error. The Otter layer immediately decodes that into
//! ordinary stdlib values such as `Path | IoError`, `str | IoError`, or
//! `null | IoError`.

use crate::strings::{LangStr, lang_str_from_utf8, str_bytes};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

fn file_registry() -> &'static Mutex<HashMap<u64, std::fs::File>> {
    static R: OnceLock<Mutex<HashMap<u64, std::fs::File>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_file_registry() -> MutexGuard<'static, HashMap<u64, std::fs::File>> {
    file_registry()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

unsafe fn read_lang_str(s: *const LangStr) -> String {
    String::from_utf8_lossy(unsafe { str_bytes(s) }).into_owned()
}

fn make_lang_str(s: &str) -> *const LangStr {
    unsafe { lang_str_from_utf8(s.as_ptr(), s.len()) }
}

fn encode_success(payload: &str) -> *const LangStr {
    make_lang_str(&encode_success_string(payload))
}

fn encode_success_string(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 1);
    out.push('0');
    out.push_str(payload);
    out
}

fn encode_error(error: impl std::fmt::Display) -> *const LangStr {
    make_lang_str(&encode_error_string(error))
}

fn encode_error_string(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    let mut out = String::with_capacity(message.len() + 1);
    out.push('1');
    out.push_str(&message);
    out
}

fn encode_bool(value: bool) -> *const LangStr {
    encode_success(if value { "1" } else { "0" })
}

fn encode_u64(value: u64) -> *const LangStr {
    encode_success(&value.to_string())
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

fn encode_bytes_hex(bytes: &[u8]) -> *const LangStr {
    let payload = bytes_hex_payload(bytes);
    encode_success(&payload)
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
            "odd-length filesystem byte payload",
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = decode_hex_digit(bytes[i]).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid hexadecimal digit in filesystem byte payload",
            )
        })?;
        let lo = decode_hex_digit(bytes[i + 1]).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid hexadecimal digit in filesystem byte payload",
            )
        })?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn path_query(
    path: *const LangStr,
    query: impl FnOnce(&Path) -> std::io::Result<bool>,
) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    match query(Path::new(&path)) {
        Ok(value) => encode_bool(value),
        Err(err) => encode_error(err),
    }
}

fn path_command(
    path: *const LangStr,
    command: impl FnOnce(&Path) -> std::io::Result<()>,
) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    match command(Path::new(&path)) {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

fn two_path_command(
    from: *const LangStr,
    to: *const LangStr,
    command: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> *const LangStr {
    let from = unsafe { read_lang_str(from) };
    let to = unsafe { read_lang_str(to) };
    match command(Path::new(&from), Path::new(&to)) {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

fn with_file_handle<T>(
    handle: i64,
    f: impl FnOnce(&mut std::fs::File) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let id = u64::try_from(handle).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file handle")
    })?;
    let mut files = lock_file_registry();
    let file = files.get_mut(&id).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file handle")
    })?;
    f(file)
}

fn register_file(file: std::fs::File) -> *const LangStr {
    let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    lock_file_registry().insert(id, file);
    encode_u64(id)
}

/// Read a UTF-8 text file.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_read_text(path: *const LangStr) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    match std::fs::read_to_string(&path) {
        Ok(contents) => encode_success(&contents),
        Err(err) => encode_error(err),
    }
}

fn write_text(path: *const LangStr, contents: *const LangStr, append: bool) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    let contents = unsafe { str_bytes(contents) };
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    match opts
        .open(&path)
        .and_then(|mut file| file.write_all(contents))
    {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

/// Create or truncate a UTF-8 text file and write `contents`.
///
/// # Safety
/// `path` and `contents` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_write_text(
    path: *const LangStr,
    contents: *const LangStr,
) -> *const LangStr {
    write_text(path, contents, false)
}

/// Create or append to a UTF-8 text file.
///
/// # Safety
/// `path` and `contents` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_append_text(
    path: *const LangStr,
    contents: *const LangStr,
) -> *const LangStr {
    write_text(path, contents, true)
}

/// Read a binary file and return an ASCII hex payload.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_read_bytes(path: *const LangStr) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    match std::fs::read(&path) {
        Ok(contents) => encode_bytes_hex(&contents),
        Err(err) => encode_error(err),
    }
}

/// Decode an ASCII hex payload and create or truncate a binary file.
///
/// # Safety
/// `path` and `contents_hex` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_write_bytes(
    path: *const LangStr,
    contents_hex: *const LangStr,
) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    let contents_hex = unsafe { read_lang_str(contents_hex) };
    match decode_hex_bytes(&contents_hex).and_then(|bytes| std::fs::write(&path, bytes)) {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

fn open_file(path: &str, mode: &str) -> std::io::Result<std::fs::File> {
    let mut opts = OpenOptions::new();
    match mode {
        "open" => {
            opts.read(true).write(true);
        }
        "create" => {
            opts.read(true).write(true).create(true).truncate(true);
        }
        "append" => {
            opts.read(true).append(true).create(true);
        }
        _ => {
            if mode.len() == 6 && mode.bytes().all(|b| b == b'0' || b == b'1') {
                let bytes = mode.as_bytes();
                opts.read(bytes[0] == b'1')
                    .write(bytes[1] == b'1')
                    .append(bytes[2] == b'1')
                    .truncate(bytes[3] == b'1')
                    .create(bytes[4] == b'1')
                    .create_new(bytes[5] == b'1');
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid filesystem open mode",
                ));
            }
        }
    }
    opts.open(path)
}

/// Open a descriptor-backed file and return a registry handle id.
///
/// # Safety
/// `path` and `mode` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_file_open(
    path: *const LangStr,
    mode: *const LangStr,
) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    let mode = unsafe { read_lang_str(mode) };
    match open_file(&path, &mode) {
        Ok(file) => register_file(file),
        Err(err) => encode_error(err),
    }
}

/// Close a descriptor-backed file handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_close(handle: i64) -> *const LangStr {
    make_lang_str(&fs_file_close_encoded(handle))
}

pub(crate) fn fs_file_close_encoded(handle: i64) -> String {
    let Ok(id) = u64::try_from(handle) else {
        return encode_error_string("invalid file handle");
    };
    match lock_file_registry().remove(&id) {
        Some(_) => encode_success_string(""),
        None => encode_error_string("invalid file handle"),
    }
}

/// Read up to `count` bytes from a descriptor-backed file handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_read(handle: i64, count: i64) -> *const LangStr {
    make_lang_str(&fs_file_read_encoded(handle, count))
}

pub(crate) fn fs_file_read_encoded(handle: i64, count: i64) -> String {
    if count < 0 {
        return encode_error_string("invalid read length");
    }
    match with_file_handle(handle, |file| {
        let mut buf = vec![0u8; count as usize];
        let n = file.read(&mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }) {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

/// Read all remaining bytes from a descriptor-backed file handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_read_to_end(handle: i64) -> *const LangStr {
    make_lang_str(&fs_file_read_to_end_encoded(handle))
}

pub(crate) fn fs_file_read_to_end_encoded(handle: i64) -> String {
    match with_file_handle(handle, |file| {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(buf)
    }) {
        Ok(bytes) => encode_success_string(&bytes_hex_payload(&bytes)),
        Err(err) => encode_error_string(err),
    }
}

/// Write bytes to a descriptor-backed file handle.
///
/// # Safety
/// `contents_hex` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_file_write(
    handle: i64,
    contents_hex: *const LangStr,
) -> *const LangStr {
    let contents_hex = unsafe { read_lang_str(contents_hex) };
    make_lang_str(&fs_file_write_encoded(handle, &contents_hex))
}

pub(crate) fn fs_file_write_encoded(handle: i64, contents_hex: &str) -> String {
    match decode_hex_bytes(contents_hex)
        .and_then(|bytes| with_file_handle(handle, |file| file.write(&bytes).map(|n| n as u64)))
    {
        Ok(n) => encode_success_string(&n.to_string()),
        Err(err) => encode_error_string(err),
    }
}

/// Flush a descriptor-backed file handle.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_file_flush(handle: i64) -> *const LangStr {
    make_lang_str(&fs_file_flush_encoded(handle))
}

pub(crate) fn fs_file_flush_encoded(handle: i64) -> String {
    match with_file_handle(handle, |file| file.flush()) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

/// Seek a descriptor-backed file handle and return the new offset.
///
/// # Safety
/// `mode` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_file_seek(
    handle: i64,
    mode: *const LangStr,
    offset: i64,
) -> *const LangStr {
    let mode = unsafe { read_lang_str(mode) };
    make_lang_str(&fs_file_seek_encoded(handle, &mode, offset))
}

pub(crate) fn fs_file_seek_encoded(handle: i64, mode: &str, offset: i64) -> String {
    let seek_from = match mode {
        "start" => match u64::try_from(offset) {
            Ok(offset) => std::io::SeekFrom::Start(offset),
            Err(_) => return encode_error_string("invalid seek"),
        },
        "current" => std::io::SeekFrom::Current(offset),
        "end" => std::io::SeekFrom::End(offset),
        _ => return encode_error_string("invalid seek mode"),
    };
    match with_file_handle(handle, |file| file.seek(seek_from)) {
        Ok(pos) => encode_success_string(&pos.to_string()),
        Err(err) => encode_error_string(err),
    }
}

/// Return whether a path exists. Missing paths are `false`; other host errors
/// are reported to the Otter layer as `IoError`.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_exists(path: *const LangStr) -> *const LangStr {
    path_query(path, |path| path.try_exists())
}

fn metadata_kind_query(
    path: &Path,
    check: impl FnOnce(&std::fs::Metadata) -> bool,
) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(check(&meta)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Return whether a path exists and is a regular file.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_is_file(path: *const LangStr) -> *const LangStr {
    path_query(path, |path| {
        metadata_kind_query(path, std::fs::Metadata::is_file)
    })
}

/// Return whether a path exists and is a directory.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_is_dir(path: *const LangStr) -> *const LangStr {
    path_query(path, |path| {
        metadata_kind_query(path, std::fs::Metadata::is_dir)
    })
}

fn path_metadata_query<T>(
    path: *const LangStr,
    query: impl FnOnce(&Path) -> std::io::Result<T>,
    encode: impl FnOnce(T) -> *const LangStr,
) -> *const LangStr {
    let path = unsafe { read_lang_str(path) };
    match query(Path::new(&path)) {
        Ok(value) => encode(value),
        Err(err) => encode_error(err),
    }
}

/// Return a compact file-kind tag: `file`, `dir`, `symlink`, or `other`.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_kind(path: *const LangStr) -> *const LangStr {
    path_metadata_query(
        path,
        |path| std::fs::symlink_metadata(path),
        |meta| {
            let ty = meta.file_type();
            if ty.is_symlink() {
                encode_success("symlink")
            } else if ty.is_file() {
                encode_success("file")
            } else if ty.is_dir() {
                encode_success("dir")
            } else {
                encode_success("other")
            }
        },
    )
}

/// Return file length in bytes.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_len(path: *const LangStr) -> *const LangStr {
    path_metadata_query(
        path,
        |path| std::fs::metadata(path),
        |meta| encode_u64(meta.len()),
    )
}

/// Return whether the path's permissions are read-only.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_read_only(path: *const LangStr) -> *const LangStr {
    path_metadata_query(
        path,
        |path| std::fs::metadata(path),
        |meta| encode_bool(meta.permissions().readonly()),
    )
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// Return whether the path has any executable permission bit on Unix targets.
/// Non-Unix providers currently report `false`.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_executable(path: *const LangStr) -> *const LangStr {
    path_metadata_query(
        path,
        |path| std::fs::metadata(path),
        |meta| encode_bool(is_executable(&meta)),
    )
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_dir() {
        std::fs::remove_dir(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Remove a regular file, symlink, or empty directory.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_remove(path: *const LangStr) -> *const LangStr {
    path_command(path, remove_path)
}

/// Rename or move a path.
///
/// # Safety
/// `from` and `to` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_rename(
    from: *const LangStr,
    to: *const LangStr,
) -> *const LangStr {
    two_path_command(from, to, |from, to| std::fs::rename(from, to))
}

/// Create a single directory.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_create_dir(path: *const LangStr) -> *const LangStr {
    path_command(path, |path| std::fs::create_dir(path))
}

/// Create a directory and all missing parents.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_create_dir_all(path: *const LangStr) -> *const LangStr {
    path_command(path, |path| std::fs::create_dir_all(path))
}

/// Resolve a path to the provider's canonical filesystem path.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_canonicalize(path: *const LangStr) -> *const LangStr {
    path_metadata_query(
        path,
        |path| std::fs::canonicalize(path),
        |path| encode_success(&path.to_string_lossy()),
    )
}

/// Return the provider's native path separator as an untagged `str`.
#[unsafe(no_mangle)]
pub extern "C" fn lang_fs_native_separator() -> *const LangStr {
    make_lang_str(std::path::MAIN_SEPARATOR_STR)
}

fn dir_entry_kind_tag(entry: &std::fs::DirEntry) -> std::io::Result<char> {
    let ty = entry.file_type()?;
    if ty.is_symlink() {
        Ok('s')
    } else if ty.is_file() {
        Ok('f')
    } else if ty.is_dir() {
        Ok('d')
    } else {
        Ok('o')
    }
}

fn encode_read_dir_entries(path: &Path) -> std::io::Result<String> {
    let mut out = String::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let tag = dir_entry_kind_tag(&entry)?;
        let path = entry.path().to_string_lossy().into_owned();
        out.push(tag);
        out.push_str(&path.chars().count().to_string());
        out.push(':');
        out.push_str(&path);
    }
    Ok(out)
}

/// Return a length-prefixed snapshot of directory entries.
///
/// # Safety
/// `path` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_fs_read_dir(path: *const LangStr) -> *const LangStr {
    path_metadata_query(path, encode_read_dir_entries, |payload| {
        encode_success(&payload)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> String {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("otter_runtime_fs_{n}.txt"))
            .to_string_lossy()
            .into_owned()
    }

    fn temp_dir_path() -> std::path::PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("otter_runtime_fs_dir_{n}"))
    }

    fn lang(s: &str) -> *const LangStr {
        unsafe { lang_str_from_utf8(s.as_ptr(), s.len()) }
    }

    fn decode(s: *const LangStr) -> String {
        String::from_utf8_lossy(unsafe { str_bytes(s) }).into_owned()
    }

    #[test]
    fn text_round_trip_and_append() {
        let path = temp_path();
        let p = lang(&path);
        assert_eq!(decode(unsafe { lang_fs_write_text(p, lang("hi")) }), "0");
        assert_eq!(decode(unsafe { lang_fs_append_text(p, lang("!")) }), "0");
        assert_eq!(decode(unsafe { lang_fs_read_text(p) }), "0hi!");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn binary_round_trip_uses_hex_payload() {
        let path = temp_path();
        let p = lang(&path);
        assert_eq!(
            decode(unsafe { lang_fs_write_bytes(p, lang("00ff4142")) }),
            "0"
        );
        assert_eq!(std::fs::read(&path).unwrap(), vec![0, 255, 65, 66]);
        assert_eq!(decode(unsafe { lang_fs_read_bytes(p) }), "000ff4142");
        assert!(decode(unsafe { lang_fs_write_bytes(p, lang("0x")) }).starts_with('1'));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_handle_hooks_read_write_seek_and_close() {
        let path = temp_path();
        let p = lang(&path);

        let opened = decode(unsafe { lang_fs_file_open(p, lang("create")) });
        assert!(
            opened.starts_with('0'),
            "expected handle success, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();

        assert_eq!(
            decode(unsafe { lang_fs_file_write(handle, lang("00ff4142")) }),
            "04"
        );
        assert_eq!(decode(lang_fs_file_flush(handle)), "0");
        assert_eq!(
            decode(unsafe { lang_fs_file_seek(handle, lang("start"), 0) }),
            "00"
        );
        assert_eq!(decode(lang_fs_file_read(handle, 2)), "000ff");
        assert_eq!(decode(lang_fs_file_read_to_end(handle)), "04142");
        assert_eq!(decode(lang_fs_file_close(handle)), "0");
        assert!(
            decode(lang_fs_file_read(handle, 1)).starts_with('1'),
            "closed handle should report an error"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn encoded_file_helpers_match_descriptor_contract() {
        let path = temp_path();
        let p = lang(&path);

        let opened = decode(unsafe { lang_fs_file_open(p, lang("create")) });
        assert!(
            opened.starts_with('0'),
            "expected handle success, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();

        assert_eq!(fs_file_write_encoded(handle, "41424344"), "04");
        assert_eq!(fs_file_flush_encoded(handle), "0");
        assert_eq!(fs_file_seek_encoded(handle, "start", 0), "00");
        assert_eq!(fs_file_read_encoded(handle, 2), "04142");
        assert_eq!(fs_file_read_to_end_encoded(handle), "04344");
        assert_eq!(fs_file_close_encoded(handle), "0");
        assert!(
            fs_file_read_to_end_encoded(handle).starts_with('1'),
            "closed handle should report an error"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_handle_open_options_payload_controls_creation_and_append() {
        let path = temp_path();
        let p = lang(&path);

        let opened = decode(unsafe { lang_fs_file_open(p, lang("110110")) });
        assert!(opened.starts_with('0'), "expected create success: {opened}");
        let handle: i64 = opened[1..].parse().unwrap();
        assert_eq!(
            decode(unsafe { lang_fs_file_write(handle, lang("4142")) }),
            "02"
        );
        assert_eq!(decode(lang_fs_file_close(handle)), "0");

        let appended = decode(unsafe { lang_fs_file_open(p, lang("101010")) });
        assert!(
            appended.starts_with('0'),
            "expected append success: {appended}"
        );
        let append_handle: i64 = appended[1..].parse().unwrap();
        assert_eq!(
            decode(unsafe { lang_fs_file_write(append_handle, lang("43")) }),
            "01"
        );
        assert_eq!(decode(lang_fs_file_close(append_handle)), "0");
        assert_eq!(std::fs::read(&path).unwrap(), b"ABC");

        assert!(
            decode(unsafe { lang_fs_file_open(p, lang("010001")) }).starts_with('1'),
            "create_new should fail for an existing path"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_registry_lock_recovers_after_poison() {
        let path = temp_path();
        std::fs::write(&path, b"abc").unwrap();

        let poisoner = std::thread::spawn(|| {
            let _guard = file_registry().lock().unwrap();
            panic!("poison file registry");
        });
        assert!(poisoner.join().is_err());
        assert!(
            file_registry().lock().is_err(),
            "test setup must leave the file registry poisoned"
        );

        let opened = decode(unsafe { lang_fs_file_open(lang(&path), lang("open")) });
        assert!(
            opened.starts_with('0'),
            "open should recover from registry poison, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();
        assert_eq!(decode(lang_fs_file_read_to_end(handle)), "0616263");
        assert_eq!(decode(lang_fs_file_close(handle)), "0");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_missing_file_reports_error() {
        let path = temp_path();
        let out = decode(unsafe { lang_fs_read_text(lang(&path)) });
        assert!(out.starts_with('1'), "expected error tag, got {out:?}");
    }

    #[test]
    fn path_queries_distinguish_file_dir_and_missing() {
        let file = temp_path();
        let dir = temp_dir_path();
        std::fs::write(&file, b"hi").unwrap();
        std::fs::create_dir(&dir).unwrap();
        let missing = temp_path();

        assert_eq!(decode(unsafe { lang_fs_exists(lang(&file)) }), "01");
        assert_eq!(decode(unsafe { lang_fs_is_file(lang(&file)) }), "01");
        assert_eq!(decode(unsafe { lang_fs_is_dir(lang(&file)) }), "00");

        let dir_s = dir.to_string_lossy();
        assert_eq!(decode(unsafe { lang_fs_exists(lang(&dir_s)) }), "01");
        assert_eq!(decode(unsafe { lang_fs_is_file(lang(&dir_s)) }), "00");
        assert_eq!(decode(unsafe { lang_fs_is_dir(lang(&dir_s)) }), "01");

        assert_eq!(decode(unsafe { lang_fs_exists(lang(&missing)) }), "00");
        assert_eq!(decode(unsafe { lang_fs_is_file(lang(&missing)) }), "00");
        assert_eq!(decode(unsafe { lang_fs_is_dir(lang(&missing)) }), "00");

        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn mutation_hooks_create_rename_and_remove_paths() {
        let root = temp_dir_path();
        let parent = root.join("a");
        let nested = parent.join("b");
        let file = nested.join("first.txt");
        let renamed = nested.join("second.txt");

        let root_s = root.to_string_lossy();
        let parent_s = parent.to_string_lossy();
        let nested_s = nested.to_string_lossy();
        let file_s = file.to_string_lossy();
        let renamed_s = renamed.to_string_lossy();

        assert_eq!(decode(unsafe { lang_fs_create_dir(lang(&root_s)) }), "0");
        assert_eq!(
            decode(unsafe { lang_fs_create_dir_all(lang(&nested_s)) }),
            "0"
        );
        assert_eq!(
            decode(unsafe { lang_fs_write_text(lang(&file_s), lang("hi")) }),
            "0"
        );
        assert_eq!(
            decode(unsafe { lang_fs_rename(lang(&file_s), lang(&renamed_s)) }),
            "0"
        );
        assert_eq!(decode(unsafe { lang_fs_exists(lang(&file_s)) }), "00");
        assert_eq!(decode(unsafe { lang_fs_exists(lang(&renamed_s)) }), "01");
        assert_eq!(decode(unsafe { lang_fs_remove(lang(&renamed_s)) }), "0");
        assert_eq!(decode(unsafe { lang_fs_remove(lang(&nested_s)) }), "0");
        assert_eq!(decode(unsafe { lang_fs_remove(lang(&parent_s)) }), "0");
        assert_eq!(decode(unsafe { lang_fs_remove(lang(&root_s)) }), "0");
        assert_eq!(decode(unsafe { lang_fs_exists(lang(&root_s)) }), "00");

        let _ = std::fs::remove_file(&renamed);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn canonicalize_hook_resolves_existing_paths_and_errors_on_missing() {
        let root = temp_dir_path();
        let nested = root.join("a").join("b");
        let file = nested.join("value.txt");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(&file, b"hi").unwrap();

        let file_s = file.to_string_lossy();
        let canonical = decode(unsafe { lang_fs_canonicalize(lang(&file_s)) });
        assert!(
            canonical.starts_with('0'),
            "expected success tag, got {canonical:?}"
        );
        assert!(
            Path::new(&canonical[1..]).is_absolute(),
            "expected absolute canonical path, got {canonical:?}"
        );

        let missing = nested.join("missing.txt");
        let missing_s = missing.to_string_lossy();
        let missing_out = decode(unsafe { lang_fs_canonicalize(lang(&missing_s)) });
        assert!(
            missing_out.starts_with('1'),
            "expected missing-path error, got {missing_out:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_separator_hook_reports_provider_separator() {
        assert_eq!(
            decode(lang_fs_native_separator()),
            std::path::MAIN_SEPARATOR_STR
        );
    }

    #[test]
    fn read_dir_hook_reports_snapshot_entries() {
        let root = temp_dir_path();
        let file = root.join("file.txt");
        let dir = root.join("child");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, b"hi").unwrap();

        let root_s = root.to_string_lossy();
        let out = decode(unsafe { lang_fs_read_dir(lang(&root_s)) });
        assert!(out.starts_with('0'), "expected success tag, got {out:?}");
        assert!(out.contains('f'), "expected file entry tag, got {out:?}");
        assert!(
            out.contains('d'),
            "expected directory entry tag, got {out:?}"
        );

        let missing = root.join("missing");
        let missing_s = missing.to_string_lossy();
        let missing_out = decode(unsafe { lang_fs_read_dir(lang(&missing_s)) });
        assert!(
            missing_out.starts_with('1'),
            "expected missing-directory error, got {missing_out:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_hooks_report_kind_len_and_permissions() {
        let file = temp_path();
        std::fs::write(&file, b"hello").unwrap();
        let p = lang(&file);

        assert_eq!(decode(unsafe { lang_fs_kind(p) }), "0file");
        assert_eq!(decode(unsafe { lang_fs_len(p) }), "05");
        let read_only = decode(unsafe { lang_fs_read_only(p) });
        assert!(read_only == "00" || read_only == "01");
        let executable = decode(unsafe { lang_fs_executable(p) });
        assert!(executable == "00" || executable == "01");

        let missing = temp_path();
        assert!(decode(unsafe { lang_fs_kind(lang(&missing)) }).starts_with('1'));

        let _ = std::fs::remove_file(file);
    }
}
