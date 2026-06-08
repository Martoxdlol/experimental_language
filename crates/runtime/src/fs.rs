//! Runtime filesystem hooks for the target-backed `std:fs` slices.
//!
//! The public API is authored in `stdlib_src/std/fs.otter`. These hooks expose a
//! small filesystem boundary over the stable runtime `str` ABI. Results are
//! encoded as a private tagged string: `"0" + payload` for success and
//! `"1" + message` for an error. The Otter layer immediately decodes that into
//! ordinary stdlib values such as `Path | IoError`, `str | IoError`, or
//! `null | IoError`.

use crate::strings::{LangStr, lang_str_from_utf8};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

type SharedFile = Arc<Mutex<std::fs::File>>;

fn file_registry() -> &'static Mutex<HashMap<u64, SharedFile>> {
    static R: OnceLock<Mutex<HashMap<u64, SharedFile>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_file_registry() -> MutexGuard<'static, HashMap<u64, SharedFile>> {
    file_registry()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

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

fn encode_bool_string(value: bool) -> String {
    encode_success_string(if value { "1" } else { "0" })
}

fn encode_u64_string(value: u64) -> String {
    encode_success_string(&value.to_string())
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

fn path_query_encoded(path: String, query: impl FnOnce(&Path) -> std::io::Result<bool>) -> String {
    match crate::gc::native_wait(|| query(Path::new(&path))) {
        Ok(value) => encode_bool_string(value),
        Err(err) => encode_error_string(err),
    }
}

fn path_command_encoded(
    path: String,
    command: impl FnOnce(&Path) -> std::io::Result<()>,
) -> String {
    match crate::gc::native_wait(|| command(Path::new(&path))) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

fn two_path_command_encoded(
    from: String,
    to: String,
    command: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> String {
    match crate::gc::native_wait(|| command(Path::new(&from), Path::new(&to))) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

fn with_file_handle<T>(
    handle: i64,
    f: impl FnOnce(&mut std::fs::File) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let id = u64::try_from(handle).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file handle")
    })?;
    let file = lock_file_registry().get(&id).cloned().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file handle")
    })?;
    let mut file = file
        .lock()
        .map_err(|_| std::io::Error::other("file handle lock poisoned"))?;
    crate::gc::native_wait(|| f(&mut file))
}

fn register_file(file: std::fs::File) -> String {
    let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    lock_file_registry().insert(id, Arc::new(Mutex::new(file)));
    encode_success_string(&id.to_string())
}

pub(crate) fn fs_read_text_encoded(path: String) -> String {
    let result = crate::gc::native_wait(|| std::fs::read_to_string(&path));
    match result {
        Ok(contents) => encode_success_string(&contents),
        Err(err) => encode_error_string(err),
    }
}

fn write_text_encoded(path: String, contents: String, append: bool) -> String {
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    let result = crate::gc::native_wait(|| {
        opts.open(&path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
    });
    match result {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn fs_write_text_encoded(path: String, contents: String) -> String {
    write_text_encoded(path, contents, false)
}

pub(crate) fn fs_append_text_encoded(path: String, contents: String) -> String {
    write_text_encoded(path, contents, true)
}

pub(crate) fn fs_read_bytes_encoded(path: String) -> String {
    let result = crate::gc::native_wait(|| std::fs::read(&path));
    match result {
        Ok(contents) => encode_success_string(&bytes_hex_payload(&contents)),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn fs_write_bytes_encoded(path: String, contents_hex: String) -> String {
    let result = decode_hex_bytes(&contents_hex)
        .and_then(|bytes| crate::gc::native_wait(|| std::fs::write(&path, bytes)));
    match result {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
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

pub(crate) fn fs_file_open_encoded(path: String, mode: String) -> String {
    let result = crate::gc::native_wait(|| open_file(&path, &mode));
    match result {
        Ok(file) => register_file(file),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn fs_file_close_encoded(handle: i64) -> String {
    let Ok(id) = u64::try_from(handle) else {
        return encode_error_string("invalid file handle");
    };
    let removed = {
        let mut registry = lock_file_registry();
        registry.remove(&id)
    };
    match removed {
        Some(file) => {
            drop_file_handle_native_wait(file);
            encode_success_string("")
        }
        None => encode_error_string("invalid file handle"),
    }
}

fn drop_file_handle_native_wait(file: SharedFile) {
    crate::gc::native_wait(|| drop(file));
}

#[cfg(test)]
pub(crate) fn test_file_handle_registered(handle: i64) -> bool {
    let Ok(id) = u64::try_from(handle) else {
        return false;
    };
    lock_file_registry().contains_key(&id)
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

pub(crate) fn fs_file_write_encoded(handle: i64, contents_hex: &str) -> String {
    match decode_hex_bytes(contents_hex)
        .and_then(|bytes| with_file_handle(handle, |file| file.write(&bytes).map(|n| n as u64)))
    {
        Ok(n) => encode_success_string(&n.to_string()),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn fs_file_flush_encoded(handle: i64) -> String {
    match with_file_handle(handle, |file| file.flush()) {
        Ok(()) => encode_success_string(""),
        Err(err) => encode_error_string(err),
    }
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

pub(crate) fn fs_exists_encoded(path: String) -> String {
    path_query_encoded(path, |path| path.try_exists())
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

pub(crate) fn fs_is_file_encoded(path: String) -> String {
    path_query_encoded(path, |path| {
        metadata_kind_query(path, std::fs::Metadata::is_file)
    })
}

pub(crate) fn fs_is_dir_encoded(path: String) -> String {
    path_query_encoded(path, |path| {
        metadata_kind_query(path, std::fs::Metadata::is_dir)
    })
}

fn path_metadata_query_encoded<T>(
    path: String,
    query: impl FnOnce(&Path) -> std::io::Result<T>,
    encode: impl FnOnce(T) -> String,
) -> String {
    match crate::gc::native_wait(|| query(Path::new(&path))) {
        Ok(value) => encode(value),
        Err(err) => encode_error_string(err),
    }
}

pub(crate) fn fs_kind_encoded(path: String) -> String {
    path_metadata_query_encoded(
        path,
        |path| std::fs::symlink_metadata(path),
        |meta| {
            let ty = meta.file_type();
            if ty.is_symlink() {
                encode_success_string("symlink")
            } else if ty.is_file() {
                encode_success_string("file")
            } else if ty.is_dir() {
                encode_success_string("dir")
            } else {
                encode_success_string("other")
            }
        },
    )
}

pub(crate) fn fs_len_encoded(path: String) -> String {
    path_metadata_query_encoded(
        path,
        |path| std::fs::metadata(path),
        |meta| encode_u64_string(meta.len()),
    )
}

pub(crate) fn fs_read_only_encoded(path: String) -> String {
    path_metadata_query_encoded(
        path,
        |path| std::fs::metadata(path),
        |meta| encode_bool_string(meta.permissions().readonly()),
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

pub(crate) fn fs_executable_encoded(path: String) -> String {
    path_metadata_query_encoded(
        path,
        |path| std::fs::metadata(path),
        |meta| encode_bool_string(is_executable(&meta)),
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

pub(crate) fn fs_remove_encoded(path: String) -> String {
    path_command_encoded(path, remove_path)
}

pub(crate) fn fs_rename_encoded(from: String, to: String) -> String {
    two_path_command_encoded(from, to, |from, to| std::fs::rename(from, to))
}

pub(crate) fn fs_create_dir_encoded(path: String) -> String {
    path_command_encoded(path, |path| std::fs::create_dir(path))
}

pub(crate) fn fs_create_dir_all_encoded(path: String) -> String {
    path_command_encoded(path, |path| std::fs::create_dir_all(path))
}

pub(crate) fn fs_canonicalize_encoded(path: String) -> String {
    path_metadata_query_encoded(
        path,
        |path| std::fs::canonicalize(path),
        |path| encode_success_string(&path.to_string_lossy()),
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

pub(crate) fn fs_read_dir_encoded(path: String) -> String {
    path_metadata_query_encoded(path, encode_read_dir_entries, |payload| {
        encode_success_string(&payload)
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

    fn decode(s: *const LangStr) -> String {
        String::from_utf8_lossy(unsafe { crate::strings::str_bytes(s) }).into_owned()
    }

    #[test]
    fn text_round_trip_and_append() {
        let path = temp_path();
        assert_eq!(fs_write_text_encoded(path.clone(), "hi".to_string()), "0");
        assert_eq!(fs_append_text_encoded(path.clone(), "!".to_string()), "0");
        assert_eq!(fs_read_text_encoded(path.clone()), "0hi!");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn binary_round_trip_uses_hex_payload() {
        let path = temp_path();
        assert_eq!(
            fs_write_bytes_encoded(path.clone(), "00ff4142".to_string()),
            "0"
        );
        assert_eq!(std::fs::read(&path).unwrap(), vec![0, 255, 65, 66]);
        assert_eq!(fs_read_bytes_encoded(path.clone()), "000ff4142");
        assert!(fs_write_bytes_encoded(path.clone(), "0x".to_string()).starts_with('1'));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_handle_encoded_helpers_read_write_seek_and_close() {
        let path = temp_path();

        let opened = fs_file_open_encoded(path.clone(), "create".to_string());
        assert!(
            opened.starts_with('0'),
            "expected handle success, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();

        assert_eq!(fs_file_write_encoded(handle, "00ff4142"), "04");
        assert_eq!(fs_file_flush_encoded(handle), "0");
        assert_eq!(fs_file_seek_encoded(handle, "start", 0), "00");
        assert_eq!(fs_file_read_encoded(handle, 2), "000ff");
        assert_eq!(fs_file_read_to_end_encoded(handle), "04142");
        assert_eq!(fs_file_close_encoded(handle), "0");
        assert!(
            fs_file_read_encoded(handle, 1).starts_with('1'),
            "closed handle should report an error"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn encoded_file_helpers_match_descriptor_contract() {
        let path = temp_path();

        let opened = fs_file_open_encoded(path.clone(), "create".to_string());
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
    fn file_descriptor_operation_does_not_hold_registry_lock() {
        let path = temp_path();

        let opened = fs_file_open_encoded(path.clone(), "create".to_string());
        assert!(
            opened.starts_with('0'),
            "expected handle success, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();

        let result = with_file_handle(handle, |_file| {
            assert_eq!(fs_file_close_encoded(handle), "0");
            Ok(())
        });
        assert!(result.is_ok(), "descriptor operation failed: {result:?}");
        assert!(
            fs_file_flush_encoded(handle).starts_with('1'),
            "closing during an in-flight descriptor operation should remove the handle"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_close_drops_removed_handle_inside_native_state_marker() {
        let path = temp_path();

        let opened = fs_file_open_encoded(path.clone(), "create".to_string());
        assert!(
            opened.starts_with('0'),
            "expected handle success, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();

        assert_eq!(fs_file_close_encoded(handle), "0");
        assert!(
            fs_file_close_encoded(handle).starts_with('1'),
            "closing an already removed descriptor should report an error"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn file_handle_open_options_payload_controls_creation_and_append() {
        let path = temp_path();

        let opened = fs_file_open_encoded(path.clone(), "110110".to_string());
        assert!(opened.starts_with('0'), "expected create success: {opened}");
        let handle: i64 = opened[1..].parse().unwrap();
        assert_eq!(fs_file_write_encoded(handle, "4142"), "02");
        assert_eq!(fs_file_close_encoded(handle), "0");

        let appended = fs_file_open_encoded(path.clone(), "101010".to_string());
        assert!(
            appended.starts_with('0'),
            "expected append success: {appended}"
        );
        let append_handle: i64 = appended[1..].parse().unwrap();
        assert_eq!(fs_file_write_encoded(append_handle, "43"), "01");
        assert_eq!(fs_file_close_encoded(append_handle), "0");
        assert_eq!(std::fs::read(&path).unwrap(), b"ABC");

        assert!(
            fs_file_open_encoded(path.clone(), "010001".to_string()).starts_with('1'),
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

        let opened = fs_file_open_encoded(path.clone(), "open".to_string());
        assert!(
            opened.starts_with('0'),
            "open should recover from registry poison, got {opened:?}"
        );
        let handle: i64 = opened[1..].parse().unwrap();
        assert_eq!(fs_file_read_to_end_encoded(handle), "0616263");
        assert_eq!(fs_file_close_encoded(handle), "0");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_missing_file_reports_error() {
        let path = temp_path();
        let out = fs_read_text_encoded(path);
        assert!(out.starts_with('1'), "expected error tag, got {out:?}");
    }

    #[test]
    fn path_queries_distinguish_file_dir_and_missing() {
        let file = temp_path();
        let dir = temp_dir_path();
        std::fs::write(&file, b"hi").unwrap();
        std::fs::create_dir(&dir).unwrap();
        let missing = temp_path();

        assert_eq!(fs_exists_encoded(file.clone()), "01");
        assert_eq!(fs_is_file_encoded(file.clone()), "01");
        assert_eq!(fs_is_dir_encoded(file.clone()), "00");

        let dir_s = dir.to_string_lossy();
        assert_eq!(fs_exists_encoded(dir_s.to_string()), "01");
        assert_eq!(fs_is_file_encoded(dir_s.to_string()), "00");
        assert_eq!(fs_is_dir_encoded(dir_s.to_string()), "01");

        assert_eq!(fs_exists_encoded(missing.clone()), "00");
        assert_eq!(fs_is_file_encoded(missing.clone()), "00");
        assert_eq!(fs_is_dir_encoded(missing), "00");

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

        assert_eq!(fs_create_dir_encoded(root_s.to_string()), "0");
        assert_eq!(fs_create_dir_all_encoded(nested_s.to_string()), "0");
        assert_eq!(
            fs_write_text_encoded(file_s.to_string(), "hi".to_string()),
            "0"
        );
        assert_eq!(
            fs_rename_encoded(file_s.to_string(), renamed_s.to_string()),
            "0"
        );
        assert_eq!(fs_exists_encoded(file_s.to_string()), "00");
        assert_eq!(fs_exists_encoded(renamed_s.to_string()), "01");
        assert_eq!(fs_remove_encoded(renamed_s.to_string()), "0");
        assert_eq!(fs_remove_encoded(nested_s.to_string()), "0");
        assert_eq!(fs_remove_encoded(parent_s.to_string()), "0");
        assert_eq!(fs_remove_encoded(root_s.to_string()), "0");
        assert_eq!(fs_exists_encoded(root_s.to_string()), "00");

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
        let canonical = fs_canonicalize_encoded(file_s.to_string());
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
        let missing_out = fs_canonicalize_encoded(missing_s.to_string());
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
        let out = fs_read_dir_encoded(root_s.to_string());
        assert!(out.starts_with('0'), "expected success tag, got {out:?}");
        assert!(out.contains('f'), "expected file entry tag, got {out:?}");
        assert!(
            out.contains('d'),
            "expected directory entry tag, got {out:?}"
        );

        let missing = root.join("missing");
        let missing_s = missing.to_string_lossy();
        let missing_out = fs_read_dir_encoded(missing_s.to_string());
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

        assert_eq!(fs_kind_encoded(file.clone()), "0file");
        assert_eq!(fs_len_encoded(file.clone()), "05");
        let read_only = fs_read_only_encoded(file.clone());
        assert!(read_only == "00" || read_only == "01");
        let executable = fs_executable_encoded(file.clone());
        assert!(executable == "00" || executable == "01");

        let missing = temp_path();
        assert!(fs_kind_encoded(missing).starts_with('1'));

        let _ = std::fs::remove_file(file);
    }
}
