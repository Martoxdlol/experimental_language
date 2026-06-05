//! Runtime process/environment hooks for `std:process`.
//!
//! The public API is authored in `stdlib_src/std/process.otter`; this module
//! exposes only target-backed process state and run-to-completion commands
//! through stable string payloads.

use crate::strings::{LangStr, lang_str_from_utf8, str_bytes};
use std::collections::HashMap;
use std::process::{Child as OsChild, Command};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

unsafe fn read_lang_str(s: *const LangStr) -> String {
    String::from_utf8_lossy(unsafe { str_bytes(s) }).into_owned()
}

fn make_lang_str(s: &str) -> *const LangStr {
    unsafe { lang_str_from_utf8(s.as_ptr(), s.len()) }
}

fn encode_success(payload: &str) -> *const LangStr {
    let mut out = String::with_capacity(payload.len() + 1);
    out.push('0');
    out.push_str(payload);
    make_lang_str(&out)
}

fn encode_error(error: impl std::fmt::Display) -> *const LangStr {
    let message = error.to_string();
    let mut out = String::with_capacity(message.len() + 1);
    out.push('1');
    out.push_str(&message);
    make_lang_str(&out)
}

fn push_len_field(out: &mut String, text: &str) {
    out.push_str(&text.len().to_string());
    out.push(':');
    out.push_str(text);
}

fn encode_bytes_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn validate_env_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        Err("environment variable name must not be empty")
    } else if name.contains('=') {
        Err("environment variable name must not contain '='")
    } else if name.contains('\0') {
        Err("environment variable name must not contain NUL")
    } else {
        Ok(())
    }
}

fn validate_env_value(value: &str) -> Result<(), &'static str> {
    if value.contains('\0') {
        Err("environment variable value must not contain NUL")
    } else {
        Ok(())
    }
}

struct FieldReader<'a> {
    payload: &'a str,
    pos: usize,
}

impl<'a> FieldReader<'a> {
    fn new(payload: &'a str) -> Self {
        Self { payload, pos: 0 }
    }

    fn read_field(&mut self) -> Result<&'a str, String> {
        let rest = self
            .payload
            .get(self.pos..)
            .ok_or_else(|| "malformed process command payload".to_string())?;
        let colon = rest
            .find(':')
            .ok_or_else(|| "malformed process command payload".to_string())?;
        let len_text = &rest[..colon];
        if len_text.is_empty() || !len_text.bytes().all(|b| b.is_ascii_digit()) {
            return Err("malformed process command payload".to_string());
        }
        let len: usize = len_text
            .parse()
            .map_err(|_| "malformed process command payload".to_string())?;
        let value_start = self.pos + colon + 1;
        let value_end = value_start
            .checked_add(len)
            .ok_or_else(|| "malformed process command payload".to_string())?;
        let value = self
            .payload
            .get(value_start..value_end)
            .ok_or_else(|| "malformed process command payload".to_string())?;
        self.pos = value_end;
        Ok(value)
    }

    fn finish(self) -> Result<(), String> {
        if self.pos == self.payload.len() {
            Ok(())
        } else {
            Err("malformed process command payload".to_string())
        }
    }
}

fn parse_usize(text: &str, label: &str) -> Result<usize, String> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("malformed {label} in process command payload"));
    }
    text.parse()
        .map_err(|_| format!("malformed {label} in process command payload"))
}

fn command_from_payload(payload: &str) -> Result<Command, String> {
    let mut fields = FieldReader::new(payload);
    let program = fields.read_field()?;
    let mut command = Command::new(program);

    let arg_count = parse_usize(fields.read_field()?, "argument count")?;
    for _ in 0..arg_count {
        command.arg(fields.read_field()?);
    }

    match fields.read_field()? {
        "inherit" => {}
        "clear" => {
            command.env_clear();
        }
        "replace" => {
            command.env_clear();
            let env_count = parse_usize(fields.read_field()?, "environment count")?;
            for _ in 0..env_count {
                let key = fields.read_field()?;
                let value = fields.read_field()?;
                validate_env_name(key).map_err(|err| err.to_string())?;
                validate_env_value(value).map_err(|err| err.to_string())?;
                command.env(key, value);
            }
        }
        _ => return Err("malformed environment mode in process command payload".to_string()),
    }

    match fields.read_field()? {
        "none" => {}
        "some" => {
            command.current_dir(fields.read_field()?);
        }
        _ => return Err("malformed cwd mode in process command payload".to_string()),
    }

    fields.finish()?;
    Ok(command)
}

fn push_status_fields(out: &mut String, status: std::process::ExitStatus) {
    let code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_default();
    push_len_field(out, &code);

    #[cfg(unix)]
    let signal = status
        .signal()
        .map(|signal| signal.to_string())
        .unwrap_or_default();
    #[cfg(not(unix))]
    let signal = String::new();
    push_len_field(out, &signal);
}

fn encode_status(status: std::process::ExitStatus) -> *const LangStr {
    let mut payload = String::new();
    push_status_fields(&mut payload, status);
    encode_success(&payload)
}

struct ChildEntry {
    child: Option<OsChild>,
    status: Option<std::process::ExitStatus>,
    waiting: bool,
}

static NEXT_CHILD_HANDLE: AtomicI64 = AtomicI64::new(1);
static CHILDREN: OnceLock<(Mutex<HashMap<i64, ChildEntry>>, Condvar)> = OnceLock::new();

fn child_table() -> &'static (Mutex<HashMap<i64, ChildEntry>>, Condvar) {
    CHILDREN.get_or_init(|| (Mutex::new(HashMap::new()), Condvar::new()))
}

fn insert_child(child: OsChild) -> i64 {
    let handle = NEXT_CHILD_HANDLE.fetch_add(1, Ordering::Relaxed);
    let (children, _) = child_table();
    let mut children = children.lock().expect("process child table poisoned");
    children.insert(
        handle,
        ChildEntry {
            child: Some(child),
            status: None,
            waiting: false,
        },
    );
    handle
}

fn encode_child_handle(handle: i64, id: u32) -> *const LangStr {
    let mut payload = String::new();
    push_len_field(&mut payload, &handle.to_string());
    push_len_field(&mut payload, &id.to_string());
    encode_success(&payload)
}

/// Snapshot the current process argument vector.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_args() -> *const LangStr {
    let mut payload = String::new();
    for arg in std::env::args() {
        push_len_field(&mut payload, &arg);
    }
    encode_success(&payload)
}

/// Read one environment variable as UTF-8/Unicode-lossy text.
///
/// # Safety
/// `name` must be a valid runtime `str` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_env(name: *const LangStr) -> *const LangStr {
    let name = unsafe { read_lang_str(name) };
    if let Err(err) = validate_env_name(&name) {
        return encode_error(err);
    }
    match std::env::var_os(&name) {
        Some(value) => {
            let value = value.to_string_lossy();
            let mut payload = String::from("1");
            payload.push_str(&value);
            encode_success(&payload)
        }
        None => encode_success("0"),
    }
}

/// Snapshot the current process environment.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_env_all() -> *const LangStr {
    let mut payload = String::new();
    for (key, value) in std::env::vars_os() {
        push_len_field(&mut payload, &key.to_string_lossy());
        push_len_field(&mut payload, &value.to_string_lossy());
    }
    encode_success(&payload)
}

/// Set one process environment variable.
///
/// # Safety
/// `name` and `value` must be valid runtime `str` pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_set_env(
    name: *const LangStr,
    value: *const LangStr,
) -> *const LangStr {
    let name = unsafe { read_lang_str(name) };
    let value = unsafe { read_lang_str(value) };
    if let Err(err) = validate_env_name(&name) {
        return encode_error(err);
    }
    if let Err(err) = validate_env_value(&value) {
        return encode_error(err);
    }
    unsafe { std::env::set_var(name, value) };
    encode_success("")
}

/// Run a command to completion without capturing stdout/stderr.
///
/// # Safety
/// `payload` must be a valid runtime `str` pointer encoded by the Otter
/// `std:process` layer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_status(payload: *const LangStr) -> *const LangStr {
    let payload = unsafe { read_lang_str(payload) };
    match command_from_payload(&payload)
        .and_then(|mut command| command.status().map_err(|err| err.to_string()))
    {
        Ok(status) => encode_status(status),
        Err(err) => encode_error(err),
    }
}

/// Run a command to completion and capture stdout/stderr.
///
/// # Safety
/// `payload` must be a valid runtime `str` pointer encoded by the Otter
/// `std:process` layer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_output(payload: *const LangStr) -> *const LangStr {
    let payload = unsafe { read_lang_str(payload) };
    match command_from_payload(&payload)
        .and_then(|mut command| command.output().map_err(|err| err.to_string()))
    {
        Ok(output) => {
            let mut payload = String::new();
            push_status_fields(&mut payload, output.status);
            push_len_field(&mut payload, &encode_bytes_hex(&output.stdout));
            push_len_field(&mut payload, &encode_bytes_hex(&output.stderr));
            encode_success(&payload)
        }
        Err(err) => encode_error(err),
    }
}

/// Spawn a live child process and return a runtime child handle plus provider id.
///
/// # Safety
/// `payload` must be a valid runtime `str` pointer encoded by the Otter
/// `std:process` layer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lang_process_spawn(payload: *const LangStr) -> *const LangStr {
    let payload = unsafe { read_lang_str(payload) };
    match command_from_payload(&payload)
        .and_then(|mut command| command.spawn().map_err(|err| err.to_string()))
    {
        Ok(child) => {
            let id = child.id();
            let handle = insert_child(child);
            encode_child_handle(handle, id)
        }
        Err(err) => encode_error(err),
    }
}

/// Wait for a live child process, caching the observed exit status.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_child_wait(handle: i64) -> *const LangStr {
    let mut child = {
        let (children, ready) = child_table();
        let mut children = children.lock().expect("process child table poisoned");
        loop {
            let Some(entry) = children.get_mut(&handle) else {
                return encode_error("unknown process child handle");
            };
            if let Some(status) = entry.status {
                return encode_status(status);
            }
            if entry.waiting {
                children = ready
                    .wait(children)
                    .expect("process child table poisoned while waiting");
                continue;
            }
            let Some(child) = entry.child.take() else {
                return encode_error("process child has no live handle");
            };
            entry.waiting = true;
            break child;
        }
    };

    let waited = child.wait();
    let (children, ready) = child_table();
    let mut children = children.lock().expect("process child table poisoned");
    let Some(entry) = children.get_mut(&handle) else {
        ready.notify_all();
        return match waited {
            Ok(status) => encode_status(status),
            Err(err) => encode_error(err),
        };
    };
    entry.waiting = false;
    let result = match waited {
        Ok(status) => {
            entry.status = Some(status);
            encode_status(status)
        }
        Err(err) => {
            entry.child = Some(child);
            encode_error(err)
        }
    };
    ready.notify_all();
    result
}

/// Kill a live child process. Waiting afterwards observes the target status.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_child_kill(handle: i64) -> *const LangStr {
    let (children, _) = child_table();
    let mut children = children.lock().expect("process child table poisoned");
    let Some(entry) = children.get_mut(&handle) else {
        return encode_error("unknown process child handle");
    };
    if entry.status.is_some() {
        return encode_success("");
    }
    if entry.waiting {
        return encode_error("process child wait already in progress");
    }
    let Some(child) = entry.child.as_mut() else {
        return encode_error("process child has no live handle");
    };
    match child.kill() {
        Ok(()) => encode_success(""),
        Err(err) => encode_error(err),
    }
}

/// Release a runtime child table entry.
///
/// Dropping the Otter handle does not kill the OS process; it mirrors
/// `std::process::Child` ownership semantics and only releases the runtime
/// registry entry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_child_release(handle: i64) {
    let (children, ready) = child_table();
    let mut children = children.lock().expect("process child table poisoned");
    children.remove(&handle);
    ready.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe fn decode(ptr: *const LangStr) -> String {
        String::from_utf8_lossy(unsafe { str_bytes(ptr) }).to_string()
    }

    fn field(text: &str) -> String {
        format!("{}:{text}", text.len())
    }

    #[test]
    fn args_and_env_hooks_return_tagged_payloads() {
        assert!(unsafe { decode(lang_process_args()) }.starts_with('0'));
        assert!(unsafe { decode(lang_process_env_all()) }.starts_with('0'));
    }

    #[test]
    fn env_hook_reports_missing_and_present_values() {
        assert_eq!(
            unsafe { decode(lang_process_env(make_lang_str("OTTER_FUSION_TEST_MISSING"))) },
            "00"
        );
        assert_eq!(
            unsafe {
                decode(lang_process_set_env(
                    make_lang_str("OTTER_FUSION_TEST_ENV"),
                    make_lang_str("ok"),
                ))
            },
            "0"
        );
        assert_eq!(
            unsafe { decode(lang_process_env(make_lang_str("OTTER_FUSION_TEST_ENV"))) },
            "01ok"
        );
    }

    #[test]
    fn invalid_env_names_are_errors() {
        assert!(unsafe { decode(lang_process_env(make_lang_str("BAD=NAME"))) }.starts_with('1'));
        assert!(
            unsafe {
                decode(lang_process_set_env(
                    make_lang_str("BAD=NAME"),
                    make_lang_str("x"),
                ))
            }
            .starts_with('1')
        );
    }

    #[test]
    fn status_and_output_run_commands_to_completion() {
        #[cfg(unix)]
        let program = "/usr/bin/true".to_string();
        #[cfg(not(unix))]
        let program = std::env::current_exe()
            .expect("current test exe")
            .to_string_lossy()
            .into_owned();

        let mut payload = String::new();
        payload.push_str(&field(&program));
        payload.push_str(&field("0"));
        payload.push_str(&field("inherit"));
        payload.push_str(&field("none"));
        let status = unsafe { decode(lang_process_status(make_lang_str(&payload))) };
        assert!(status.starts_with('0'), "{status}");

        #[cfg(unix)]
        let payload = {
            let mut payload = String::new();
            payload.push_str(&field("/bin/echo"));
            payload.push_str(&field("1"));
            payload.push_str(&field("otter"));
            payload.push_str(&field("inherit"));
            payload.push_str(&field("none"));
            payload
        };
        #[cfg(not(unix))]
        {
            payload.clear();
            payload.push_str(&field(&program));
            payload.push_str(&field("1"));
            payload.push_str(&field("--list"));
            payload.push_str(&field("inherit"));
            payload.push_str(&field("none"));
        }

        let output = unsafe { decode(lang_process_output(make_lang_str(&payload))) };
        assert!(output.starts_with('0'), "{output}");
    }

    #[test]
    fn spawn_wait_and_release_child_handles() {
        #[cfg(unix)]
        let program = "/usr/bin/true".to_string();
        #[cfg(not(unix))]
        let program = std::env::current_exe()
            .expect("current test exe")
            .to_string_lossy()
            .into_owned();

        let mut payload = String::new();
        payload.push_str(&field(&program));
        payload.push_str(&field("0"));
        payload.push_str(&field("inherit"));
        payload.push_str(&field("none"));
        let spawned = unsafe { decode(lang_process_spawn(make_lang_str(&payload))) };
        assert!(spawned.starts_with('0'), "{spawned}");

        let mut fields = FieldReader::new(&spawned[1..]);
        let handle: i64 = fields
            .read_field()
            .expect("handle field")
            .parse()
            .expect("numeric handle");
        assert!(handle > 0);
        let child_id: u32 = fields
            .read_field()
            .expect("id field")
            .parse()
            .expect("numeric child id");
        assert!(child_id > 0);
        fields.finish().expect("exact payload");

        let first_wait = unsafe { decode(lang_process_child_wait(handle)) };
        assert!(first_wait.starts_with('0'), "{first_wait}");
        let second_wait = unsafe { decode(lang_process_child_wait(handle)) };
        assert_eq!(first_wait, second_wait);

        lang_process_child_release(handle);
        let after_release = unsafe { decode(lang_process_child_wait(handle)) };
        assert!(after_release.starts_with('1'), "{after_release}");
    }

    #[test]
    #[cfg(unix)]
    fn kill_live_child_then_waits() {
        let mut payload = String::new();
        payload.push_str(&field("/bin/sleep"));
        payload.push_str(&field("1"));
        payload.push_str(&field("5"));
        payload.push_str(&field("inherit"));
        payload.push_str(&field("none"));
        let spawned = unsafe { decode(lang_process_spawn(make_lang_str(&payload))) };
        assert!(spawned.starts_with('0'), "{spawned}");

        let mut fields = FieldReader::new(&spawned[1..]);
        let handle: i64 = fields
            .read_field()
            .expect("handle field")
            .parse()
            .expect("numeric handle");
        let _ = fields.read_field().expect("id field");
        fields.finish().expect("exact payload");

        let killed = unsafe { decode(lang_process_child_kill(handle)) };
        assert_eq!(killed, "0");
        let waited = unsafe { decode(lang_process_child_wait(handle)) };
        assert!(waited.starts_with('0'), "{waited}");
        lang_process_child_release(handle);
    }
}
