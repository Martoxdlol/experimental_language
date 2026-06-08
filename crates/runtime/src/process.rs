//! Runtime process/environment hooks for `std:process`.
//!
//! The public API is authored in `stdlib_src/std/process.otter`; this module
//! exposes only private target-backed process state and command helpers through
//! stable string payloads. Public Otter Fusion source reaches wait-capable
//! operations through async futures in `async_rt`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::{Child as OsChild, Command};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

fn encode_success_text(payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 1);
    out.push('0');
    out.push_str(payload);
    out
}

fn encode_error_text(error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    let mut out = String::with_capacity(message.len() + 1);
    out.push('1');
    out.push_str(&message);
    out
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

    #[cfg(unix)]
    let core_dumped = if status.core_dumped() { "1" } else { "0" };
    #[cfg(not(unix))]
    let core_dumped = "";
    push_len_field(out, core_dumped);

    #[cfg(unix)]
    let stopped_signal = status
        .stopped_signal()
        .map(|signal| signal.to_string())
        .unwrap_or_default();
    #[cfg(not(unix))]
    let stopped_signal = String::new();
    push_len_field(out, &stopped_signal);

    #[cfg(unix)]
    let continued = if status.continued() { "1" } else { "0" };
    #[cfg(not(unix))]
    let continued = "";
    push_len_field(out, continued);
}

fn encode_status_text(status: std::process::ExitStatus) -> String {
    let mut payload = String::new();
    push_status_fields(&mut payload, status);
    encode_success_text(&payload)
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

fn wait_child_table<'a>(
    ready: &Condvar,
    children: std::sync::MutexGuard<'a, HashMap<i64, ChildEntry>>,
) -> std::sync::MutexGuard<'a, HashMap<i64, ChildEntry>> {
    crate::gc::native_wait(|| {
        ready
            .wait(children)
            .expect("process child table poisoned while waiting")
    })
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

fn drop_child_entry_native_wait(entry: ChildEntry) {
    crate::gc::native_wait(|| drop(entry));
}

fn drop_os_child_native_wait(child: OsChild) {
    crate::gc::native_wait(|| drop(child));
}

fn encode_child_handle_text(handle: i64, id: u32) -> String {
    let mut payload = String::new();
    push_len_field(&mut payload, &handle.to_string());
    push_len_field(&mut payload, &id.to_string());
    encode_success_text(&payload)
}

fn process_args_native_wait() -> Vec<String> {
    crate::gc::native_wait(|| std::env::args().collect())
}

fn process_env_native_wait(name: &str) -> Option<OsString> {
    crate::gc::native_wait(|| std::env::var_os(name))
}

fn process_env_all_native_wait() -> Vec<(OsString, OsString)> {
    crate::gc::native_wait(|| std::env::vars_os().collect())
}

fn process_set_env_native_wait(name: String, value: String) {
    crate::gc::native_wait(|| unsafe { std::env::set_var(name, value) });
}

pub(crate) fn process_args_encoded() -> String {
    let mut payload = String::new();
    for arg in process_args_native_wait() {
        push_len_field(&mut payload, &arg);
    }
    encode_success_text(&payload)
}

pub(crate) fn process_env_encoded(name: String) -> String {
    if let Err(err) = validate_env_name(&name) {
        return encode_error_text(err);
    }
    match process_env_native_wait(&name) {
        Some(value) => {
            let value = value.to_string_lossy();
            let mut payload = String::from("1");
            payload.push_str(&value);
            encode_success_text(&payload)
        }
        None => encode_success_text("0"),
    }
}

pub(crate) fn process_env_all_encoded() -> String {
    let mut payload = String::new();
    for (key, value) in process_env_all_native_wait() {
        push_len_field(&mut payload, &key.to_string_lossy());
        push_len_field(&mut payload, &value.to_string_lossy());
    }
    encode_success_text(&payload)
}

pub(crate) fn process_set_env_encoded(name: String, value: String) -> String {
    if let Err(err) = validate_env_name(&name) {
        return encode_error_text(err);
    }
    if let Err(err) = validate_env_value(&value) {
        return encode_error_text(err);
    }
    process_set_env_native_wait(name, value);
    encode_success_text("")
}

pub(crate) fn process_status_encoded(payload: String) -> String {
    let result = command_from_payload(&payload).and_then(|mut command| {
        crate::gc::native_wait(|| command.status()).map_err(|err| err.to_string())
    });
    match result {
        Ok(status) => encode_status_text(status),
        Err(err) => encode_error_text(err),
    }
}

pub(crate) fn process_output_encoded(payload: String) -> String {
    let result = command_from_payload(&payload).and_then(|mut command| {
        crate::gc::native_wait(|| command.output()).map_err(|err| err.to_string())
    });
    match result {
        Ok(output) => {
            let mut payload = String::new();
            push_status_fields(&mut payload, output.status);
            push_len_field(&mut payload, &encode_bytes_hex(&output.stdout));
            push_len_field(&mut payload, &encode_bytes_hex(&output.stderr));
            encode_success_text(&payload)
        }
        Err(err) => encode_error_text(err),
    }
}

pub(crate) fn process_spawn_encoded(payload: String) -> String {
    let result = command_from_payload(&payload).and_then(|mut command| {
        crate::gc::native_wait(|| command.spawn()).map_err(|err| err.to_string())
    });
    match result {
        Ok(child) => {
            let id = child.id();
            let handle = insert_child(child);
            encode_child_handle_text(handle, id)
        }
        Err(err) => encode_error_text(err),
    }
}

pub(crate) fn process_child_wait_encoded(handle: i64) -> String {
    let mut child = {
        let (children, ready) = child_table();
        let mut children = children.lock().expect("process child table poisoned");
        loop {
            let Some(entry) = children.get_mut(&handle) else {
                return encode_error_text("unknown process child handle");
            };
            if let Some(status) = entry.status {
                return encode_status_text(status);
            }
            if entry.waiting {
                children = wait_child_table(ready, children);
                continue;
            }
            let Some(child) = entry.child.take() else {
                return encode_error_text("process child has no live handle");
            };
            entry.waiting = true;
            break child;
        }
    };

    let waited = crate::gc::native_wait(|| child.wait());
    let (children, ready) = child_table();
    let mut children = children.lock().expect("process child table poisoned");
    let Some(entry) = children.get_mut(&handle) else {
        ready.notify_all();
        let result = match waited {
            Ok(status) => encode_status_text(status),
            Err(err) => encode_error_text(err),
        };
        drop_os_child_native_wait(child);
        return result;
    };
    entry.waiting = false;
    let result = match waited {
        Ok(status) => {
            entry.status = Some(status);
            encode_status_text(status)
        }
        Err(err) => {
            entry.child = Some(child);
            encode_error_text(err)
        }
    };
    ready.notify_all();
    result
}

pub(crate) fn process_child_kill_encoded(handle: i64) -> String {
    let mut child = {
        let (children, _) = child_table();
        let mut children = children.lock().expect("process child table poisoned");
        let Some(entry) = children.get_mut(&handle) else {
            return encode_error_text("unknown process child handle");
        };
        if entry.status.is_some() {
            return encode_success_text("");
        }
        if entry.waiting {
            return encode_error_text("process child wait already in progress");
        }
        let Some(child) = entry.child.take() else {
            return encode_error_text("process child has no live handle");
        };
        entry.waiting = true;
        child
    };

    let killed = crate::gc::native_wait(|| child.kill());
    let (children, ready) = child_table();
    let mut children = children.lock().expect("process child table poisoned");
    let Some(entry) = children.get_mut(&handle) else {
        ready.notify_all();
        let result = match killed {
            Ok(()) => encode_success_text(""),
            Err(err) => encode_error_text(err),
        };
        drop_os_child_native_wait(child);
        return result;
    };
    entry.waiting = false;
    entry.child = Some(child);
    let result = match killed {
        Ok(()) => encode_success_text(""),
        Err(err) => encode_error_text(err),
    };
    ready.notify_all();
    result
}

/// Release a runtime child table entry.
///
/// Dropping the Otter handle does not kill the OS process; it mirrors
/// `std::process::Child` ownership semantics and only releases the runtime
/// registry entry.
#[unsafe(no_mangle)]
pub extern "C" fn lang_process_child_release(handle: i64) {
    let (children, ready) = child_table();
    let removed = {
        let mut children = children.lock().expect("process child table poisoned");
        children.remove(&handle)
    };
    ready.notify_all();
    if let Some(entry) = removed {
        drop_child_entry_native_wait(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(text: &str) -> String {
        format!("{}:{text}", text.len())
    }

    #[test]
    fn args_and_env_hooks_return_tagged_payloads() {
        assert!(process_args_encoded().starts_with('0'));
        assert!(process_env_all_encoded().starts_with('0'));
    }

    #[test]
    fn process_environment_helpers_use_native_state_marker() {
        assert!(!process_args_native_wait().is_empty());
        let pairs = process_env_all_native_wait();
        assert_eq!(
            process_env_native_wait("OTTER_FUSION_PROCESS_HELPER_TEST_MISSING"),
            None
        );
        assert!(pairs.iter().all(|(key, _)| !key.is_empty()));
    }

    #[test]
    fn process_set_env_helper_uses_native_state_marker() {
        process_set_env_native_wait(
            "OTTER_FUSION_PROCESS_HELPER_TEST_ENV".to_string(),
            "ok".to_string(),
        );
        assert_eq!(
            process_env_native_wait("OTTER_FUSION_PROCESS_HELPER_TEST_ENV"),
            Some(OsString::from("ok"))
        );
    }

    #[test]
    fn env_hook_reports_missing_and_present_values() {
        assert_eq!(
            process_env_encoded("OTTER_FUSION_TEST_MISSING".to_string()),
            "00"
        );
        assert_eq!(
            process_set_env_encoded("OTTER_FUSION_TEST_ENV".to_string(), "ok".to_string()),
            "0"
        );
        assert_eq!(
            process_env_encoded("OTTER_FUSION_TEST_ENV".to_string()),
            "01ok"
        );
    }

    #[test]
    fn invalid_env_names_are_errors() {
        assert!(process_env_encoded("BAD=NAME".to_string()).starts_with('1'));
        assert!(process_set_env_encoded("BAD=NAME".to_string(), "x".to_string()).starts_with('1'));
    }

    #[test]
    fn status_and_output_resolve_command_futures() {
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
        let status = process_status_encoded(payload.clone());
        assert!(status.starts_with('0'), "{status}");
        let mut fields = FieldReader::new(&status[1..]);
        assert_eq!(fields.read_field().expect("status code"), "0");
        assert_eq!(fields.read_field().expect("status signal"), "");
        #[cfg(unix)]
        assert_eq!(fields.read_field().expect("status core dumped"), "0");
        #[cfg(not(unix))]
        assert_eq!(fields.read_field().expect("status core dumped"), "");
        assert_eq!(fields.read_field().expect("status stopped signal"), "");
        #[cfg(unix)]
        assert_eq!(fields.read_field().expect("status continued"), "0");
        #[cfg(not(unix))]
        assert_eq!(fields.read_field().expect("status continued"), "");
        fields.finish().expect("exact status payload");

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

        let output = process_output_encoded(payload);
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
        let spawned = process_spawn_encoded(payload);
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

        let first_wait = process_child_wait_encoded(handle);
        assert!(first_wait.starts_with('0'), "{first_wait}");
        let second_wait = process_child_wait_encoded(handle);
        assert_eq!(first_wait, second_wait);

        lang_process_child_release(handle);
        let after_release = process_child_wait_encoded(handle);
        assert!(after_release.starts_with('1'), "{after_release}");
    }

    #[test]
    fn release_live_child_removes_entry_inside_native_state_marker() {
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
        let spawned = process_spawn_encoded(payload);
        assert!(spawned.starts_with('0'), "{spawned}");

        let mut fields = FieldReader::new(&spawned[1..]);
        let handle: i64 = fields
            .read_field()
            .expect("handle field")
            .parse()
            .expect("numeric handle");
        let _ = fields.read_field().expect("id field");
        fields.finish().expect("exact payload");

        lang_process_child_release(handle);
        let after_release = process_child_wait_encoded(handle);
        assert!(after_release.starts_with('1'), "{after_release}");
    }

    #[test]
    fn removed_during_wait_or_kill_drops_local_child_inside_native_state_marker() {
        let source = include_str!("process.rs");
        let wait_body = source
            .split("pub(crate) fn process_child_wait_encoded(handle: i64) -> String")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) fn process_child_kill_encoded")
                    .next()
            })
            .expect("process_child_wait_encoded should remain in process.rs");
        let kill_body = source
            .split("pub(crate) fn process_child_kill_encoded(handle: i64) -> String")
            .nth(1)
            .and_then(|rest| rest.split("/// Release a runtime child table entry").next())
            .expect("process_child_kill_encoded should remain in process.rs");

        assert!(
            source.contains("fn drop_os_child_native_wait(child: OsChild)")
                && source.contains("crate::gc::native_wait(|| drop(child))"),
            "local provider Child drops must have a GC native-state helper"
        );
        assert!(
            wait_body.contains("drop_os_child_native_wait(child);"),
            "Child.wait's removed-entry path must drop the local provider child through the helper"
        );
        assert!(
            kill_body.contains("drop_os_child_native_wait(child);"),
            "Child.kill's removed-entry path must drop the local provider child through the helper"
        );
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
        let spawned = process_spawn_encoded(payload);
        assert!(spawned.starts_with('0'), "{spawned}");

        let mut fields = FieldReader::new(&spawned[1..]);
        let handle: i64 = fields
            .read_field()
            .expect("handle field")
            .parse()
            .expect("numeric handle");
        let _ = fields.read_field().expect("id field");
        fields.finish().expect("exact payload");

        let killed = process_child_kill_encoded(handle);
        assert_eq!(killed, "0");
        let waited = process_child_wait_encoded(handle);
        assert!(waited.starts_with('0'), "{waited}");
        lang_process_child_release(handle);
    }
}
