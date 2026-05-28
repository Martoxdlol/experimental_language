//! End-to-end tests: write a `.lang` file, invoke the `lang` binary, and check
//! its stdout/exit status. Exercises the full pipeline including `print`.

use std::process::Command;

/// Run `lang <cmd> <file>` with `src` written to a temp file; return
/// (stdout, stderr, success).
fn lang(cmd: &str, src: &str) -> (String, String, bool) {
    lang_env(cmd, src, &[])
}

/// Like [`lang`], with extra command-line flags after the file (e.g.
/// `--release`).
fn lang_flag(cmd: &str, src: &str, flags: &[&str]) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_test_{}.lang", nonce()));
    std::fs::write(&path, src).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lang"));
    command.arg(cmd).arg(&path);
    for f in flags {
        command.arg(f);
    }
    let out = command.output().expect("run lang");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Like [`lang`], with extra environment variables.
fn lang_env(cmd: &str, src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("lang_test_{}.lang", nonce()));
    std::fs::write(&path, src).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lang"));
    command.arg(cmd).arg(&path);
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().expect("run lang");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Compile `src` to a native executable with `lang build -o`, run it, and
/// return (stdout, stderr, success). Exercises the object-emit + link path.
fn lang_build_run(src: &str, env: &[(&str, &str)]) -> (String, String, bool) {
    let dir = std::env::temp_dir();
    let n = nonce();
    let path = dir.join(format!("lang_test_{n}.lang"));
    let exe = dir.join(format!("lang_test_bin_{n}"));
    std::fs::write(&path, src).unwrap();
    let build = Command::new(env!("CARGO_BIN_EXE_lang"))
        .arg("build").arg(&path).arg("-o").arg(&exe)
        .output()
        .expect("run lang build");
    let _ = std::fs::remove_file(&path);
    if !build.status.success() {
        return (
            String::new(),
            String::from_utf8_lossy(&build.stderr).into_owned(),
            false,
        );
    }
    let mut run = Command::new(&exe);
    for (k, v) in env {
        run.env(k, v);
    }
    let out = run.output().expect("run native executable");
    let _ = std::fs::remove_file(&exe);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Write a multi-file program into a fresh temp directory and run it.
/// `files` maps a relative path (e.g. `"app/util.lang"`) to its source; the
/// entry is `entry` (relative to the temp dir). Returns (stdout, stderr, ok).
fn lang_run_project(entry: &str, files: &[(&str, &str)]) -> (String, String, bool) {
    let root = std::env::temp_dir().join(format!("lang_proj_{}", nonce()));
    for (rel, src) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, src).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_lang"))
        .arg("run")
        .arg(root.join(entry))
        .output()
        .expect("run lang");
    let _ = std::fs::remove_dir_all(&root);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

fn nonce() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = C.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 ^ (n << 32)
}

#[test]
fn hello_world() {
    let (out, err, ok) = lang("run", "function main() { println(\"hello, world\") }");
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello, world\n");
}

#[test]
fn print_no_newline() {
    let (out, _, ok) = lang("run", "function main() { print(\"a\"); print(\"b\") }");
    assert!(ok);
    assert_eq!(out, "ab");
}

#[test]
fn print_computed_number() {
    let src = "function main() { println(\"answer: \" + (fib(10) as str)) }\n\
               function fib(n: i64): i64 { if n < 2 { n } else { fib(n-1) + fib(n-2) } }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "answer: 55\n");
}

#[test]
fn structs_end_to_end() {
    let src = "struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var p = Point { x: 40, y: 2 };\n\
                 println((p.x + p.y) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn divide_by_zero_panics() {
    // A non-constant zero divisor traps the runtime panic path.
    let src = "function main() { var d: i64 = 0; var x: i64 = 10 / d; println(x as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok, "expected a panic; stdout={out}");
    assert!(err.contains("divide by zero"), "stderr: {err}");
}

#[test]
fn error_propagation_with_question_mark() {
    let src = "struct User { name: str }\n\
               function find(id: i64): User | str { if id == 1 { User { name: \"A\" } } else { \"nf\" } }\n\
               function greet(id: i64): str { var u: User = find(id)?; \"hi \" + u.name }\n\
               function main() { println(greet(1)); println(greet(2)); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hi A\nnf\n");
}

#[test]
fn gc_collects_garbage_keeping_live_roots() {
    // With collection forced on every allocation (`LANG_GC=stress`), the live
    // `keep` string and `b` struct must survive the loop's garbage allocations.
    // A missed root would free them and corrupt the output.
    let src = "struct Box { v: i64 }\n\
               function main() {\n\
                 var keep = \"important data\";\n\
                 var b = Box { v: 100 };\n\
                 var total: i64 = 0;\n\
                 var i: i64 = 0;\n\
                 while i < 300 {\n\
                   var garbage = [i, i, i];\n\
                   var s = \"tmp\" + (i as str);\n\
                   total = total + garbage[1] + b.v;\n\
                   i = i + 1;\n\
                 }\n\
                 println(keep);\n\
                 println(total as str);\n\
                 println(b.v as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // total = sum over i in 0..300 of (i + 100) = (0+1+...+299) + 300*100
    let expected_total: i64 = (0..300).sum::<i64>() + 300 * 100;
    assert_eq!(out, format!("important data\n{expected_total}\n100\n"));
}

#[test]
fn map_survives_gc_stress() {
    // A `Map<str, str>` whose keys and values are heap strings must keep all of
    // its entries alive across the loop's garbage allocations under stress GC.
    let src = "function main() {\n\
                 var m: Map<str, str> = { \"name\": \"ada\", \"lang\": \"rust\" };\n\
                 var i: i64 = 0;\n\
                 while i < 200 {\n\
                   var garbage = \"junk\" + (i as str);\n\
                   m.set(\"iter\", garbage);\n\
                   i = i + 1;\n\
                 }\n\
                 match m.get(\"name\") { str s => println(s), null => println(\"lost\") };\n\
                 match m.get(\"lang\") { str s => println(s), null => println(\"lost\") };\n\
                 println(\"size=${m.size()}\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // name/lang preserved; size = 3 (name, lang, iter).
    assert_eq!(out, "ada\nrust\nsize=3\n");
}

#[test]
fn for_over_user_iterator_under_gc_stress() {
    // The `Iterator` protocol `for` loop allocates an `Item`/union box every
    // step; under stress GC, the loop variable and the iterator must stay sound.
    let src = "struct Range { current: i64, end: i64 }\n\
               extend Range: Iterator<i64> {\n\
                 function next(self): Item<i64> | Done {\n\
                   if self.current >= self.end { Done {} }\n\
                   else { var v = self.current; self.current = self.current + 1; Item { value: v } }\n\
                 }\n\
               }\n\
               function main() {\n\
                 var acc = \"\";\n\
                 for x in (Range { current: 0, end: 5 }) {\n\
                   acc = acc + \"${x},\";\n\
                 }\n\
                 println(acc);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "0,1,2,3,4,\n");
}

#[test]
fn dyn_dispatch_heterogeneous_under_gc_stress() {
    // Interface objects box `{vtable, data}`; the GC must trace each data
    // pointer (here a struct with a heap `str` field) through the box.
    let src = "interface Show { function show(self): str; }\n\
               struct Named { name: str }\n\
               struct Anon {}\n\
               extend Named: Show { function show(self): str { self.name } }\n\
               extend Anon: Show { function show(self): str { \"anon\" } }\n\
               function main() {\n\
                 var zoo: List<Show> = [Named { name: \"ada\" }, Anon {}, Named { name: \"bob\" }];\n\
                 var out = \"\";\n\
                 for s in zoo { out = out + s.show() + \",\"; }\n\
                 println(out);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "ada,anon,bob,\n");
}

#[test]
fn check_rejects_unsatisfied_bound() {
    let src = "interface Show { function show(self): str; }\n\
               struct Cat {}\n\
               function speak<T: Show>(x: T): str { x.show() }\n\
               function main() { var s = speak(Cat {}); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("does not implement `Show`"), "stderr: {err}");
}

#[test]
fn closures_capture_survive_gc_stress() {
    // A closure's heap environment (here capturing a `str`) and the closures
    // held in a list must survive stress-mode collection.
    let src = "function tagger(tag: str): (str) => str {\n\
                 (s: str): str => tag + s\n\
               }\n\
               function main() {\n\
                 var fns: List<(str) => str> = [tagger(\"a:\"), tagger(\"b:\")];\n\
                 var out = \"\";\n\
                 var i = 0;\n\
                 while i < 100 { var junk = \"x\" + (i as str); i = i + 1; }\n\
                 for f in fns { out = out + f(\"v\") + \" \"; }\n\
                 println(out);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "a:v b:v \n");
}

#[test]
fn for_entry_in_map_under_gc_stress() {
    // `for entry in map` builds an `Entry<str,str>` (two heap fields) each step;
    // the GC must keep the map, the key snapshot, and each entry sound.
    let src = "function main() {\n\
                 var m: Map<str, str> = { \"a\": \"x\", \"b\": \"y\" };\n\
                 m[\"c\"] = \"z\";\n\
                 var out = \"\";\n\
                 for e in m { out = out + e.key + e.value; }\n\
                 println(out);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // probe order is deterministic for these keys
    assert_eq!(out, "czaxby\n");
}

#[test]
fn check_rejects_non_iterable() {
    let src = "struct P { x: i64 }\n\
               function f() { for n in (P { x: 1 }) { } }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("is not iterable"), "stderr: {err}");
}

#[test]
fn check_rejects_non_hashable_map_key() {
    let src = "function f() {\n\
                 var m: Map<bool, i64> = { true: 1 };\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("cannot be used as a map key"), "stderr: {err}");
}

#[test]
fn check_reports_type_error() {
    let (_, err, ok) = lang("check", "function f(): i64 { true }");
    assert!(!ok);
    assert!(err.contains("expected `i64`"), "stderr: {err}");
}

#[test]
fn native_build_hello_world() {
    // `lang build` emits a relocatable object, links it against the runtime
    // static library, and the resulting standalone binary prints on its own.
    let (out, err, ok) = lang_build_run(
        "function main() { println(\"hello from a native binary\") }",
        &[],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hello from a native binary\n");
}

#[test]
fn native_build_matches_jit_output() {
    // A program touching strings, structs, and recursion must produce the same
    // output whether JIT-run or compiled to a native executable.
    let src = "struct Point { x: i64, y: i64 }\n\
               function fib(n: i64): i64 { if n < 2 { n } else { fib(n-1) + fib(n-2) } }\n\
               function main() {\n\
                 var p = Point { x: 40, y: 2 };\n\
                 println(\"sum=${p.x + p.y} fib=${fib(15)}\");\n\
               }";
    let (jit_out, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat_out, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit_out, nat_out);
    assert_eq!(nat_out, "sum=42 fib=610\n");
}

#[test]
fn native_build_gc_stress_keeps_live_roots() {
    // The native entry registers each function's GC safepoints at startup
    // (function address + code offset → precise pc). Under stress collection a
    // missed root would free the live data and corrupt the output, exactly as
    // in the JIT path — this proves precise stack maps work in the linked binary.
    let src = "struct Box { v: i64 }\n\
               function main() {\n\
                 var keep = \"important data\";\n\
                 var b = Box { v: 100 };\n\
                 var total: i64 = 0;\n\
                 var i: i64 = 0;\n\
                 while i < 300 {\n\
                   var garbage = [i, i, i];\n\
                   var s = \"tmp\" + (i as str);\n\
                   total = total + garbage[1] + b.v;\n\
                   i = i + 1;\n\
                 }\n\
                 println(keep);\n\
                 println(total as str);\n\
               }";
    let (out, err, ok) = lang_build_run(src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    let expected_total: i64 = (0..300).sum::<i64>() + 300 * 100;
    assert_eq!(out, format!("important data\n{expected_total}\n"));
}

#[test]
fn native_build_panic_exits_101() {
    // A runtime panic (divide by zero) in a native binary must exit with 101,
    // the same code the runtime uses under the JIT.
    let src = "function main() { var d: i64 = 0; var x: i64 = 10 / d; println(x as str); }";
    let (_, err, ok) = lang_build_run(src, &[]);
    assert!(!ok);
    assert!(err.contains("divide by zero"), "stderr: {err}");
}

#[test]
fn integer_overflow_panics() {
    // `docs/14` §2: integer overflow panics in debug builds. `i64::MAX + 1`
    // must panic with an overflow message, not silently wrap.
    let src = "function main() {\n\
                 var x: i64 = 9223372036854775807;\n\
                 var y: i64 = x + 1;\n\
                 println(y as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok, "expected an overflow panic; stdout={out}");
    assert!(err.contains("add with overflow"), "stderr: {err}");
}

#[test]
fn multiplication_overflow_panics() {
    let src = "function main() {\n\
                 var x: i64 = 4000000000;\n\
                 var y: i64 = x * x;\n\
                 println(y as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("multiply with overflow"), "stderr: {err}");
}

#[test]
fn signed_division_overflow_panics() {
    // `INT_MIN / -1` is the one signed-division overflow; it must panic, not
    // raise a hardware trap (`docs/14` §2).
    let src = "function main() {\n\
                 var a: i64 = -9223372036854775807 - 1;\n\
                 var b: i64 = -1;\n\
                 println((a / b) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("divide with overflow"), "stderr: {err}");
}

#[test]
fn shift_past_width_panics() {
    // `docs/14` §2: a shift by `>=` the bit width always panics.
    let src = "function main() {\n\
                 var w: i64 = 64;\n\
                 var x: i64 = 1;\n\
                 var y: i64 = x << w;\n\
                 println(y as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("shift amount"), "stderr: {err}");
}

#[test]
fn valid_arithmetic_does_not_false_panic() {
    // Large-but-in-range arithmetic and shifts must compute, not panic.
    let src = "function main() {\n\
                 var a: i64 = 1000000;\n\
                 println((a * a) as str);\n\
                 println((1 << 10) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "1000000000000\n1024\n");
}

#[test]
fn panic_builtin_terminates_with_message() {
    // `docs/14`: `panic(message: str): never`. Code before the panic runs;
    // code after is unreachable; the process exits 101 with the message.
    let src = "function check(n: i64): i64 { if n < 0 { panic(\"neg: \" + (n as str)) } n }\n\
               function main() {\n\
                 println(check(5) as str);\n\
                 println(check(-2) as str);\n\
                 println(\"unreachable\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok);
    assert_eq!(out, "5\n");
    assert!(err.contains("neg: -2"), "stderr: {err}");
}

#[test]
fn panic_is_usable_in_value_position() {
    // `never` is a subtype of every type, so `panic(...)` type-checks as the
    // else-arm of an `i64`-typed `if`.
    let src = "function pos(n: i64): i64 {\n\
                 var x: i64 = if n >= 0 { n } else { panic(\"neg\") };\n\
                 x + 1\n\
               }\n\
               function main() { println(pos(41) as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn exit_builtin_sets_process_code() {
    // `docs/24`: `exit(code: i32): never`. Output before runs; the exit code
    // is the argument.
    let src = "function main() { println(\"before\"); exit(3i32); println(\"after\"); }";
    let (out, _, ok) = lang("run", src);
    assert!(!ok);
    assert_eq!(out, "before\n");
}

#[test]
fn float_to_int_in_range_truncates() {
    // `docs/14` §2: in-range float→int truncates toward zero, no panic.
    let src = "function main() {\n\
                 var f: f64 = 3.9;\n\
                 println((f as i64) as str);\n\
                 var g: f64 = -2.7;\n\
                 println((g as i64) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3\n-2\n");
}

#[test]
fn float_to_int_out_of_range_panics() {
    // A float beyond i32's range must panic, not silently wrap (`docs/14` §2).
    let src = "function main() {\n\
                 var f: f64 = 5000000000.0;\n\
                 println((f as i32) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected an out-of-range panic");
    assert!(err.contains("out of range"), "stderr: {err}");
}

#[test]
fn float_nan_to_int_panics() {
    // NaN (0.0/0.0, floats never panic on division) panics when cast to int.
    let src = "function main() {\n\
                 var z: f64 = 0.0;\n\
                 var nan: f64 = z / z;\n\
                 println((nan as i64) as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected a NaN cast panic");
    assert!(err.contains("out of range") || err.contains("NaN"), "stderr: {err}");
}

#[test]
fn int_to_char_valid_and_invalid() {
    // A valid scalar casts; an out-of-range code point panics (`docs/14` §2).
    let ok_src = "function main() {\n\
                    var n: i64 = 65;\n\
                    println((n as char) as str);\n\
                  }";
    let (out, err, ok) = lang("run", ok_src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "A\n");

    let bad_src = "function main() {\n\
                     var n: i64 = 1114112;\n\
                     var c: char = n as char;\n\
                     println(c as str);\n\
                   }";
    let (_, err, ok) = lang("run", bad_src);
    assert!(!ok, "expected an invalid-char panic");
    assert!(err.contains("Unicode"), "stderr: {err}");
}

#[test]
fn int_to_char_surrogate_panics() {
    // Surrogate code points (0xD800..=0xDFFF) are not valid scalars.
    let src = "function main() {\n\
                 var n: i64 = 55296;\n\
                 var c: char = n as char;\n\
                 println(c as str);\n\
               }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok, "expected a surrogate panic");
    assert!(err.contains("surrogate"), "stderr: {err}");
}

#[test]
fn panic_with_terminates() {
    // `docs/14` §1: `panic_with(value): never` terminates the thread; the value
    // can be any type (widened to `dynamic`); code after is unreachable.
    let src = "struct Bug { code: i64 }\n\
               function main() {\n\
                 println(\"before\");\n\
                 panic_with(Bug { code: 7 });\n\
                 println(\"after\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(!ok, "expected panic_with to terminate");
    assert_eq!(out, "before\n");
    assert!(err.contains("panic"), "stderr: {err}");
}

#[test]
fn release_profile_wraps_overflow() {
    // `docs/14` §5: in release, overflowing `+`/`-`/`*` wrap (two's complement)
    // instead of panicking. `i32::MAX + 1` wraps to `i32::MIN`.
    let src = "function main() {\n\
                 var a: i32 = 2147483647;\n\
                 println((a + 1) as str);\n\
               }";
    let (out, err, ok) = lang_flag("run", src, &["--release"]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "-2147483648\n");

    // The same program panics under the (default) debug profile.
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("overflow"), "stderr: {err}");
}

#[test]
fn release_profile_wraps_signed_div_overflow() {
    // `INT_MIN / -1` wraps to `INT_MIN` and `INT_MIN % -1` to `0` in release.
    let src = "function main() {\n\
                 var a: i64 = -9223372036854775807 - 1;\n\
                 var d: i64 = -1;\n\
                 println((a / d) as str);\n\
                 println((a % d) as str);\n\
               }";
    let (out, err, ok) = lang_flag("run", src, &["--release"]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "-9223372036854775808\n0\n");
}

#[test]
fn release_profile_still_panics_on_bad_shift() {
    // Shifts past the bit width panic in *both* profiles (`docs/14` §5).
    let src = "function main() {\n\
                 var a: i32 = 1;\n\
                 var s: i32 = 40;\n\
                 println((a << s) as str);\n\
               }";
    let (_, err, ok) = lang_flag("run", src, &["--release"]);
    assert!(!ok, "expected a shift panic even in release");
    assert!(err.contains("shift"), "stderr: {err}");
}

#[test]
fn numeric_namespace_intrinsics() {
    // `docs/18` §10, `docs/14` §5: constants and overflow-arithmetic families on
    // primitive numeric types, plus float predicates.
    let src = "function chk(r: i32 | null): str { match r { i32 v => v as str, null => \"of\" } }\n\
               function main() {\n\
                 var m: i32 = 2147483647;\n\
                 println(i32.MAX as str);\n\
                 println(i32.MIN as str);\n\
                 println(u8.MAX as str);\n\
                 println(i32.wrapping_add(m, 1) as str);\n\
                 println(i32.saturating_add(m, 9) as str);\n\
                 println(chk(i32.checked_add(m, 1)));\n\
                 println(chk(i32.checked_add(1, 2)));\n\
                 var z: f64 = 0.0;\n\
                 println(f64.is_nan(z / z) as str);\n\
                 println(f64.is_finite(f64.INFINITY) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(
        out,
        "2147483647\n-2147483648\n255\n-2147483648\n2147483647\nof\n3\ntrue\nfalse\n"
    );
}

#[test]
fn drop_finalizer_runs_on_collection() {
    // `docs/16` §8: a `Drop` impl's `drop(self)` runs when the collector
    // reclaims an unreachable object. Under stress GC each loop temporary is
    // finalized promptly; a live value is not.
    let src = "struct Resource { id: i64 }\n\
               extend Resource: Drop {\n\
                 function drop(self) { println(\"drop \" + (self.id as str)); }\n\
               }\n\
               function main() {\n\
                 var i: i64 = 0;\n\
                 while i < 3 { var r: Resource = Resource { id: i }; i = i + 1; }\n\
                 println(\"done\");\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // All three temporaries are finalized; order is deterministic single-thread.
    assert!(out.contains("drop 0"), "out: {out}");
    assert!(out.contains("drop 1"), "out: {out}");
    assert!(out.contains("drop 2"), "out: {out}");
    assert!(out.contains("done"), "out: {out}");
}

#[test]
fn shared_mutex_serializes_concurrent_increments() {
    // `docs/20` §4: `Shared<T>` is a mutex. Two threads each increment a shared
    // counter 5000 times under `lock`; no updates are lost. `join()` is async,
    // so the async main `await`s each handle's `Future<Joined<R> | Panicked>`.
    let src = "struct Counter { value: i64 }\n\
               function bump(s: Shared<Counter>, n: i64) {\n\
                 var i: i64 = 0;\n\
                 while i < n { s.lock((c) => { c.value = c.value + 1; 0 }); i = i + 1; }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var state: Shared<Counter> = Shared.new(Counter { value: 0 });\n\
                 var a: Shared<Counter> = state.clone();\n\
                 var b: Shared<Counter> = state.clone();\n\
                 var h1: JoinHandle<i64> = Thread.spawn(() => { bump(a, 5000); 0 });\n\
                 var h2: JoinHandle<i64> = Thread.spawn(() => { bump(b, 5000); 0 });\n\
                 var r1: Joined<i64> | Panicked = await h1.join();\n\
                 var r2: Joined<i64> | Panicked = await h2.join();\n\
                 println((state.lock((c) => c.value)) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "10000\n");
}

#[test]
fn shared_try_lock_returns_value_or_lock_busy() {
    // `try_lock` yields `R | LockBusy`; on an uncontended lock it succeeds.
    let src = "struct Box { v: i64 }\n\
               function main() {\n\
                 var s: Shared<Box> = Shared.new(Box { v: 42 });\n\
                 match s.try_lock((b) => b.v) {\n\
                   i64 n => println(\"got \" + (n as str)),\n\
                   LockBusy busy => println(\"busy\"),\n\
                 }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "got 42\n");
}

#[test]
fn channel_cross_thread_producer_consumer() {
    // `docs/20` §2: a worker thread sends over a channel; main consumes
    // asynchronously. `recv()` is a `Future<T>` — `await`ing it suspends the
    // task (an async main here) instead of blocking the thread. The `Sender`
    // is captured into the spawned closure (a thread-safe handle).
    let src = "function produce(tx: Sender<i64>) {\n\
                 var i: i64 = 1;\n\
                 while i <= 5 { tx.send(i * 10); i = i + 1; }\n\
               }\n\
               function consume(rx: Receiver<i64>): Future<i64> async {\n\
                 var total: i64 = 0; var n: i64 = 0;\n\
                 while n < 5 { var m: i64 = await rx.recv(); total = total + m; n = n + 1; }\n\
                 total\n\
               }\n\
               function main(): Future<null> async {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { produce(tx); 0 });\n\
                 var total: i64 = await consume(rx);\n\
                 var r: Joined<i64> | Panicked = await h.join();\n\
                 println(total as str);\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out1, "150\n"); // 10+20+30+40+50
    let (out2, _, ok2) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok2);
    assert_eq!(out1, out2, "GC stress changed the channel result");
}

#[test]
fn channel_async_recv_of_managed_element_survives_gc_stress() {
    // `docs/20` §2: an async `recv()` of a *managed* element (`str`). The
    // message rides the channel queue (pinned as a GC root) and is moved into
    // the future's `Ready<str>` traced value slot when polled — so a collection
    // anywhere in the hand-off must not free it. The consumer awaits each recv.
    let src = "function produce(tx: Sender<str>) {\n\
                 tx.send(\"a\"); tx.send(\"b\"); tx.send(\"c\");\n\
               }\n\
               function consume(rx: Receiver<str>): Future<str> async {\n\
                 var acc: str = \"\"; var n: i64 = 0;\n\
                 while n < 3 { var m: str = await rx.recv(); acc = acc + m; n = n + 1; }\n\
                 acc\n\
               }\n\
               function main(): Future<null> async {\n\
                 var pair: (Sender<str>, Receiver<str>) = channel<str>();\n\
                 var tx: Sender<str> = pair.0;\n\
                 var rx: Receiver<str> = pair.1;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => { produce(tx); 0 });\n\
                 var acc: str = await consume(rx);\n\
                 var r: Joined<i64> | Panicked = await h.join();\n\
                 println(acc);\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out1, "abc\n");
    let (out2, _, ok2) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok2);
    assert_eq!(out1, out2, "GC stress changed the channel result");
}

#[test]
fn channel_try_recv_is_non_blocking() {
    // `try_recv` returns `T | null`: the value if present, else `null`.
    let src = "function main() {\n\
                 var pair: (Sender<i64>, Receiver<i64>) = channel<i64>();\n\
                 var tx: Sender<i64> = pair.0;\n\
                 var rx: Receiver<i64> = pair.1;\n\
                 tx.send(7);\n\
                 match rx.try_recv() { i64 n => println(\"got \" + (n as str)), null => println(\"empty\") }\n\
                 match rx.try_recv() { i64 n => println(\"got \" + (n as str)), null => println(\"empty\") }\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "got 7\nempty\n");
}

#[test]
fn static_method_on_concrete_type() {
    // `docs/09` §6: `Type.static_method(args)` calls a static (no-`self`) method
    // declared in an `extend` of the type.
    let src = "struct Point { x: i64, y: i64 }\n\
               extend Point {\n\
                 function at(x: i64, y: i64): Point { Point { x: x, y: y } }\n\
                 function sum(self): i64 { self.x + self.y }\n\
               }\n\
               function main() { println(Point.at(3, 4).sum() as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "7\n");
}

#[test]
fn static_method_through_generic_bound() {
    // `docs/10`: `T.static_method()` for `T: Trait` resolves to the concrete
    // impl, monomorphized.
    let src = "interface Default { function default(): Self; }\n\
               struct Widget { id: i64 }\n\
               extend Widget: Default { function default(): Widget { Widget { id: 42 } } }\n\
               function make<T: Default>(): T { T.default() }\n\
               function main() { var w: Widget = make<Widget>(); println(w.id as str); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn calling_instance_method_statically_errors() {
    // An instance method (takes `self`) cannot be called as `Type.method()`.
    let src = "struct P { x: i64 }\n\
               extend P { function get(self): i64 { self.x } }\n\
               function main() { println(P.get() as str); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected an error calling an instance method statically");
    assert!(err.contains("instance method"), "stderr: {err}");
}

#[test]
fn thread_spawn_join_returns_result() {
    // `docs/20` §1: `Thread.spawn(() => R)` runs a closure on a new OS thread;
    // `join()` is async — it returns a `Future<Joined<R> | Panicked>` awaited
    // inside the async main (or any async body).
    let src = "function take(r: Joined<i64> | Panicked): i64 {\n\
                 match r { Joined<i64> j => j.value, Panicked p => 0 - 1 }\n\
               }\n\
               function main(): Future<null> async {\n\
                 var base: i64 = 10;\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => {\n\
                   var s: i64 = 0; var i: i64 = 0;\n\
                   while i < 1000 { s = s + i; i = i + 1; }\n\
                   s + base\n\
                 });\n\
                 var r: Joined<i64> | Panicked = await h.join();\n\
                 println(take(r) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "499510\n"); // sum(0..1000) + 10
}

#[test]
fn many_threads_under_gc_stress() {
    // Several workers each allocate heavily; the result must be deterministic
    // and memory-safe under `LANG_GC=stress` (stop-the-world coordination). An
    // async gatherer awaits each `join()` in turn so the main task suspends
    // (rather than parking the OS thread) between worker completions.
    let src = "function work(id: i64): i64 {\n\
                 var acc: i64 = 0; var i: i64 = 0;\n\
                 while i < 3000 { var s: str = \"n-\" + (i as str); acc = acc + s.size(); i = i + 1; }\n\
                 acc + id\n\
               }\n\
               function take(r: Joined<i64> | Panicked): i64 {\n\
                 match r { Joined<i64> j => j.value, Panicked p => 0 }\n\
               }\n\
               function gather(a: JoinHandle<i64>, b: JoinHandle<i64>, c: JoinHandle<i64>): Future<i64> async {\n\
                 var ra: Joined<i64> | Panicked = await a.join();\n\
                 var rb: Joined<i64> | Panicked = await b.join();\n\
                 var rc: Joined<i64> | Panicked = await c.join();\n\
                 take(ra) + take(rb) + take(rc)\n\
               }\n\
               function main(): Future<null> async {\n\
                 var a: JoinHandle<i64> = Thread.spawn(() => work(1));\n\
                 var b: JoinHandle<i64> = Thread.spawn(() => work(2));\n\
                 var c: JoinHandle<i64> = Thread.spawn(() => work(3));\n\
                 var total: i64 = await gather(a, b, c);\n\
                 println(total as str);\n\
               }";
    let (out1, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    let (out2, _, ok2) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok2);
    assert_eq!(out1, out2, "GC stress changed the result (memory corruption)");
}

#[test]
fn spawn_rejects_mutable_capture() {
    // A spawned closure capturing a mutable (struct) value is rejected until
    // deep-clone of captures lands (`docs/20` §1).
    let src = "struct Counter { n: i64 }\n\
               function main() {\n\
                 var c: Counter = Counter { n: 0 };\n\
                 var h: JoinHandle<i64> = Thread.spawn(() => c.n);\n\
                 println(\"unreachable\");\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected a capture rejection");
    assert!(err.contains("immutable") || err.contains("clone"), "stderr: {err}");
}

#[test]
fn derived_clone_is_a_deep_copy() {
    // `docs/15` §8: `@Derive(Clone)` is field-by-field deep copy. Mutating a
    // clone (incl. through a nested struct) must not affect the original.
    let src = "@Derive(Clone)\n\
               struct Point { x: i64, y: i64 }\n\
               @Derive(Clone)\n\
               struct Line { from: Point, to: Point }\n\
               function main() {\n\
                 var a: Line = Line { from: Point { x: 0, y: 0 }, to: Point { x: 3, y: 4 } };\n\
                 var b: Line = a.clone();\n\
                 b.to.x = 99;\n\
                 println((a.to.x as str) + \" \" + (b.to.x as str));\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3 99\n");
}

#[test]
fn clone_through_clone_bound() {
    // A `T: Clone` bound dispatches `.clone()` to the intrinsic clone for
    // builtins and to the derived impl for user types (`docs/11`/`docs/15`).
    let src = "@Derive(Clone)\n\
               struct P { x: i64 }\n\
               function dup<T: Clone>(v: T): T { v.clone() }\n\
               function main() {\n\
                 println(dup<i64>(5) as str);\n\
                 println(dup<str>(\"hi\"));\n\
                 var p: P = P { x: 10 };\n\
                 var q: P = dup<P>(p);\n\
                 q.x = 77;\n\
                 println((p.x as str) + \" \" + (q.x as str));\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "5\nhi\n10 77\n");
}

#[test]
fn generic_struct_derives_clone() {
    // `@Derive(Clone)` on a generic struct synthesises `extend<T: Clone> S<T>:
    // Clone`. The clone is a deep copy: mutating a nested field of the copy must
    // not touch the original, and a primitive payload clones intrinsically.
    let src = "@Derive(Clone)\n\
               struct Box<T> { value: T }\n\
               @Derive(Clone)\n\
               struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var a = Box { value: Point { x: 1, y: 2 } };\n\
                 var b = a.clone();\n\
                 b.value.x = 99;\n\
                 println((a.value.x as str) + \" \" + (b.value.x as str));\n\
                 var n = Box { value: 7 };\n\
                 println(n.clone().value as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "1 99\n7\n");
    // Native build must match the JIT output byte-for-byte.
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn generic_tuple_struct_construction_and_clone() {
    // Generic tuple-struct construction infers its type arguments from the
    // positional arguments (`Pair(5, "x")` → `Pair<i64, str>`), and `Clone`
    // derives on it.
    let src = "@Derive(Clone)\n\
               struct Pair<A, B>(A, B)\n\
               function main() {\n\
                 var t = Pair(5, \"x\");\n\
                 var u = t.clone();\n\
                 println((u.0 as str) + u.1);\n\
                 var p = Pair(1, 2);\n\
                 println((p.0 + p.1) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "5x\n3\n");
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn generic_struct_derives_eq_and_ord() {
    // `@Derive(Eq, Ord)` on a generic struct: per-field `==`/`<` become
    // `.eq()`/`.lt()` calls dispatched through each field's `Eq`/`Ord` bound.
    // Works for primitive fields (intrinsic compare) and user-type fields
    // (their own derived impl).
    let src = "@Derive(Eq, Ord)\n\
               struct Pair<A, B> { a: A, b: B }\n\
               @Derive(Eq, Ord)\n\
               struct Point { x: i64, y: i64 }\n\
               function main() {\n\
                 var p1 = Pair { a: 1, b: \"x\" };\n\
                 var p2 = Pair { a: 1, b: \"y\" };\n\
                 println((p1 == p1) as str);\n\
                 println((p1 < p2) as str);\n\
                 println((p2 < p1) as str);\n\
                 println((p1 != p2) as str);\n\
                 var q1 = Pair { a: Point { x: 1, y: 2 }, b: 0 };\n\
                 var q2 = Pair { a: Point { x: 1, y: 3 }, b: 0 };\n\
                 println((q1 < q2) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\ntrue\nfalse\ntrue\ntrue\n");
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn generic_derived_type_satisfies_eq_bound() {
    // A concrete `@Derive(Eq)` type satisfies a `T: Eq` bound (its synthesised
    // `extend` declares the interface), so it can be a generic struct's element.
    let src = "@Derive(Eq)\n\
               struct Wrap<T> { inner: T }\n\
               @Derive(Eq)\n\
               struct Id { n: i64 }\n\
               function main() {\n\
                 var a = Wrap { inner: Id { n: 7 } };\n\
                 var b = Wrap { inner: Id { n: 7 } };\n\
                 var c = Wrap { inner: Id { n: 9 } };\n\
                 println((a == b) as str);\n\
                 println((a == c) as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn generic_struct_derives_to_str() {
    // `@Derive(ToStr)` on a generic struct: each field is rendered via a
    // `.to_str()` call dispatched through its `ToStr` bound — a primitive field
    // via the intrinsic `as str`, a user-type field via its own `to_str`. Also
    // exercised through string interpolation.
    let src = "@Derive(ToStr)\n\
               struct Box<T> { value: T }\n\
               @Derive(ToStr)\n\
               struct Point { x: i64, y: i64 }\n\
               @Derive(ToStr)\n\
               struct Pair<A, B>(A, B)\n\
               function main() {\n\
                 println(Box { value: 7 }.to_str());\n\
                 println(Box { value: \"hi\" }.to_str());\n\
                 println(Box { value: Point { x: 1, y: 2 } }.to_str());\n\
                 println(Pair(5, \"x\").to_str());\n\
                 println(\"interp: ${Box { value: 9 }}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(
        out,
        "Box { value: 7 }\nBox { value: hi }\nBox { value: Point { x: 1, y: 2 } }\nPair(5, x)\ninterp: Box { value: 9 }\n"
    );
    let (nout, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nout, out);
}

#[test]
fn list_clone_is_independent_under_gc_stress() {
    // A cloned `List` is independent of the original, and the clone allocation
    // is GC-safe (the new handle/buffer are rooted across collection).
    let src = "function main() {\n\
                 var i: i64 = 0;\n\
                 var total: i64 = 0;\n\
                 while i < 200 {\n\
                   var xs: List<i64> = [i, i + 1];\n\
                   var ys: List<i64> = xs.clone();\n\
                   ys.push(0);\n\
                   total = total + xs.size() + ys.size();\n\
                   i = i + 1;\n\
                 }\n\
                 println(total as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    // each iteration: xs.size()=2, ys.size()=3 → 5 * 200 = 1000
    assert_eq!(out, "1000\n");
}

#[test]
fn clone_rejects_mutable_list_elements() {
    // Cloning a `List` of a mutable (struct) element type is rejected until
    // per-element deep clone lands; the diagnostic names the element type.
    let src = "struct P { x: i64 }\n\
               function main() {\n\
                 var xs: List<P> = [P { x: 1 }];\n\
                 var ys: List<P> = xs.clone();\n\
                 println(ys.size() as str);\n\
               }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok, "expected a clone rejection");
    assert!(err.contains("clone") && err.contains("List"), "stderr: {err}");
}

#[test]
fn multi_file_named_imports() {
    // `mod util;` loads `app/util.lang`; named imports bring its public function
    // and struct into the entry module's scope (`docs/17`).
    let entry = "mod util;\n\
                 import { add, Point } from \"util\";\n\
                 function main() {\n\
                   println(\"sum=${add(40, 2)}\");\n\
                   var p: Point = Point { x: 3, y: 4 };\n\
                   println(\"pt=(${p.x},${p.y})\");\n\
                 }";
    let util = "pub function add(a: i64, b: i64): i64 { a + b }\n\
                pub struct Point { x: i64, y: i64 }";
    let (out, err, ok) =
        lang_run_project("app.lang", &[("app.lang", entry), ("app/util.lang", util)]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "sum=42\npt=(3,4)\n");
}

#[test]
fn multi_file_rejects_private_import() {
    // A non-`pub` item cannot be imported across modules (`docs/17` §3).
    let entry = "mod util;\n\
                 import { secret } from \"util\";\n\
                 function main() { println(secret() as str); }";
    let util = "function secret(): i64 { 99 }";
    let (_, err, ok) =
        lang_run_project("app.lang", &[("app.lang", entry), ("app/util.lang", util)]);
    assert!(!ok);
    assert!(err.contains("`secret` is private"), "stderr: {err}");
}

#[test]
fn multi_file_strict_module_scoping() {
    // Names do not cross module boundaries without `import`: a submodule cannot
    // see a crate-root function it never imported (`docs/17` §3).
    let entry = "mod util;\n\
                 pub function root_only(): i64 { 7 }\n\
                 function main() { println(\"${root_only()}\"); }";
    let util = "pub function uses_root(): i64 { root_only() }";
    let (_, err, ok) =
        lang_run_project("app.lang", &[("app.lang", entry), ("app/util.lang", util)]);
    assert!(!ok);
    assert!(err.contains("cannot find value `root_only`"), "stderr: {err}");
}

#[test]
fn import_as_namespace_calls() {
    // `import "mathx" as M` binds a namespace; `M.foo(..)` calls the module's
    // public functions (`docs/17` §3).
    let entry = "mod mathx;\n\
                 import \"mathx\" as M;\n\
                 function main() { println(\"${M.add(40, 2)} ${M.square(7)}\"); }";
    let mathx = "pub function add(a: i64, b: i64): i64 { a + b }\n\
                 pub function square(n: i64): i64 { n * n }";
    let (out, err, ok) =
        lang_run_project("app.lang", &[("app.lang", entry), ("app/mathx.lang", mathx)]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42 49\n");
}

#[test]
fn import_as_namespace_rejects_private() {
    // Namespaced access reaches only the module's public definitions.
    let entry = "mod mathx;\n\
                 import \"mathx\" as M;\n\
                 function main() { println(\"${M.hidden()}\"); }";
    let mathx = "function hidden(): i64 { 0 }";
    let (_, err, ok) =
        lang_run_project("app.lang", &[("app.lang", entry), ("app/mathx.lang", mathx)]);
    assert!(!ok);
    assert!(err.contains("no public value `hidden`"), "stderr: {err}");
}

#[test]
fn multi_file_nested_submodule() {
    // A submodule may itself declare a file-backed submodule, loaded from a
    // directory named for its parent file's stem (`app/util/` for `util.lang`).
    let entry = "mod util;\n\
                 import { triple } from \"util\";\n\
                 function main() { println(\"${triple(5)}\"); }";
    let util = "mod math;\n\
                import { times } from \"util/math\";\n\
                pub function triple(n: i64): i64 { times(n, 3) }";
    let math = "pub function times(a: i64, b: i64): i64 { a * b }";
    let (out, err, ok) = lang_run_project(
        "app.lang",
        &[("app.lang", entry), ("app/util.lang", util), ("app/util/math.lang", math)],
    );
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "15\n");
}

#[test]
fn ffi_extern_function_call() {
    // `docs/19`: an `extern function` is called across the C ABI. `abs` from
    // libc is resolved by the JIT (dlsym) / the linker (native).
    let src = "extern function abs(n: i32): i32;\n\
               function main() { println(\"abs=${abs(-5i32)}\"); }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "abs=5\n");
}

#[test]
fn ffi_extern_call_native_matches_jit() {
    // The same FFI program must behave identically JIT-run and natively linked.
    let src = "extern function abs(n: i32): i32;\n\
               function main() { println(\"${abs(-7i32) + abs(3i32)}\"); }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "10\n");
}

#[test]
fn derive_eq_synthesizes_struct_equality() {
    // `docs/22` §11: `@Derive(Eq)` synthesises field-by-field `eq`; `==`/`!=`
    // then work on the struct (record, tuple, and unit forms).
    let src = "@Derive(Eq)\n\
               struct Point { x: i64, y: i64 }\n\
               @Derive(Eq)\n\
               struct Pair(i64, str)\n\
               @Derive(Eq)\n\
               struct Unit {}\n\
               function main() {\n\
                 println(\"${Point { x: 1, y: 2 } == Point { x: 1, y: 2 }}\");\n\
                 println(\"${Point { x: 1, y: 2 } != Point { x: 1, y: 9 }}\");\n\
                 println(\"${Pair(7, \"hi\") == Pair(7, \"hi\")}\");\n\
                 println(\"${Pair(7, \"hi\") == Pair(7, \"bye\")}\");\n\
                 println(\"${Unit {} == Unit {}}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true\ntrue\ntrue\nfalse\ntrue\n");
}

#[test]
fn derive_tostr_renders_struct() {
    // `@Derive(ToStr)` synthesises a `to_str(self): str` rendering each field.
    let src = "@Derive(ToStr)\n\
               struct Point { x: i64, y: str }\n\
               @Derive(ToStr)\n\
               struct Pair(i64, bool)\n\
               function main() {\n\
                 println(Point { x: 1, y: \"hi\" }.to_str());\n\
                 println(Pair(7, true).to_str());\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "Point { x: 1, y: hi }\nPair(7, true)\n");
}

#[test]
fn derive_ord_lexicographic_comparison() {
    // `@Derive(Ord)` synthesises lexicographic `<`/`<=`/`>`/`>=` by field
    // declaration order, and implies `Eq` (`docs/22` §11).
    let src = "@Derive(Eq, Ord)\n\
               struct Version { major: i64, minor: i64 }\n\
               function main() {\n\
                 var a = Version { major: 1, minor: 2 };\n\
                 var b = Version { major: 1, minor: 5 };\n\
                 println(\"${a < b} ${b < a} ${a <= a} ${b > a} ${a >= b} ${a == a}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "true false true true false true\n");
}

#[test]
fn logical_not_on_bool_negates() {
    // Regression: `!` on a `bool` is logical negation (0↔1), not bitwise
    // complement (which left both `!true` and `!false` truthy). `!` on an
    // integer stays bitwise (`docs/15`).
    let src = "function main() {\n\
                 var t: bool = true;\n\
                 println(\"${!t} ${!false} ${!(1 < 2)}\");\n\
                 var x: i64 = 5;\n\
                 println(\"${!x}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "false true false\n-6\n");
}

#[test]
fn interpolate_user_type_via_to_str() {
    // A user type with a `to_str(): str` method (derived or hand-written) is
    // interpolatable in a string (`docs/01` §8 ToStr protocol).
    let src = "@Derive(ToStr)\n\
               struct Point { x: i64, y: i64 }\n\
               struct Tag {}\n\
               extend Tag { function to_str(self): str { \"tag!\" } }\n\
               function main() {\n\
                 var p = Point { x: 3, y: 4 };\n\
                 println(\"p=${p} t=${Tag {}}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "p=Point { x: 3, y: 4 } t=tag!\n");
}

#[test]
fn interpolate_rejects_type_without_to_str() {
    // A struct with no `to_str` is still rejected in interpolation.
    let src = "struct Bare { x: i64 }\n\
               function main() { var b = Bare { x: 1 }; println(\"${b}\"); }";
    let (_, err, ok) = lang("run", src);
    assert!(!ok);
    assert!(err.contains("no `to_str(): str` method"), "stderr: {err}");
}

#[test]
fn question_mark_with_from_residual_conversion() {
    // `docs/13` §4: `?` converts an error variant not in the return type via a
    // `FromResidual` impl on the return's error type.
    let src = "struct Config { ok: i64 }\n\
               struct AppError { code: i64 }\n\
               struct IoError {}\n\
               struct ParseError {}\n\
               extend AppError: FromResidual<IoError> { function from_residual(r: IoError): AppError { AppError { code: 1 } } }\n\
               extend AppError: FromResidual<ParseError> { function from_residual(r: ParseError): AppError { AppError { code: 2 } } }\n\
               function load(f: bool): i64 | IoError { if f { IoError {} } else { 10 } }\n\
               function parse(f: bool): i64 | ParseError { if f { ParseError {} } else { 20 } }\n\
               function run(a: bool, b: bool): Config | AppError {\n\
                 var x: i64 = load(a)?;\n\
                 var y: i64 = parse(b)?;\n\
                 Config { ok: x + y }\n\
               }\n\
               function code(a: bool, b: bool): i64 {\n\
                 match run(a, b) { Config c => c.ok, AppError e => 0 - e.code }\n\
               }\n\
               function main() {\n\
                 println(\"${code(false, false)} ${code(true, false)} ${code(false, true)}\");\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    // success=30; IoError→-1; ParseError→-2
    assert_eq!(out, "30 -1 -2\n");
}

#[test]
fn check_clean_program_succeeds() {
    let (out, _, ok) = lang("check", "function main() { var x: i64 = 1 + 2; }");
    assert!(ok);
    assert!(out.contains("ok"));
}

// -- async (docs/21) ---------------------------------------------------------

#[test]
fn async_fn_driven_by_block_on() {
    // An async function lowers to a Future state machine; an async main awaits
    // it directly. (No `await` in `answer`'s body — the core pipeline slice.)
    let src = "function answer(): Future<i64> async { 40 + 2 }\n\
               function main(): Future<null> async {\n\
                 var x: i64 = await answer();\n\
                 println(x as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn async_block_captures_and_block_on() {
    // A bare `async { … }` block is a zero-arg inline future literal that
    // captures enclosing locals; an async main awaits it.
    let src = "function main(): Future<null> async {\n\
                 var n: i64 = 40;\n\
                 var f: i64 = await async { n + 2 };\n\
                 println(f as str);\n\
                 var g: str = await async { \"async str\" };\n\
                 println(g);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\nasync str\n");
}

#[test]
fn async_block_on_str_survives_gc_stress() {
    // The future's managed (str) result must survive collections triggered
    // during the poll/await machinery.
    let src = "function greet(name: str): Future<str> async { \"hi, \" + name }\n\
               function main(): Future<null> async {\n\
                 var s: str = await greet(\"world\");\n\
                 println(s);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "hi, world\n");
}

#[test]
fn async_native_build_matches_jit() {
    let src = "function answer(): Future<i64> async { 42 }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await answer();\n\
                 println(v as str);\n\
               }";
    let (jit, jerr, jok) = lang("run", src);
    assert!(jok, "jit stderr: {jerr}");
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(jit, nat);
    assert_eq!(nat, "42\n");
}

#[test]
fn await_chains_async_functions() {
    // `outer` awaits `inner` twice; each await polls the inner future, unwraps
    // its Ready value, and continues the state machine.
    let src = "function inner(): Future<i64> async { 5 }\n\
               function outer(): Future<i64> async {\n\
                 var x: i64 = await inner();\n\
                 var y: i64 = await inner();\n\
                 x + y + 1\n\
               }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await outer();\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "11\n");
}

#[test]
fn await_in_control_flow_and_managed_results() {
    let src = "function fetch(name: str): Future<str> async { \"data:\" + name }\n\
               function small(): Future<i64> async { 7 }\n\
               function pick(c: bool): Future<i64> async {\n\
                 var r: i64 = 0;\n\
                 if c { var a: i64 = await small(); r = a + 10; } else { r = await small(); }\n\
                 r\n\
               }\n\
               function gather(): Future<str> async {\n\
                 var s: str = await fetch(\"x\");\n\
                 var n: i64 = await pick(true);\n\
                 \"${s} n=${n}\"\n\
               }\n\
               function main(): Future<null> async {\n\
                 var s: str = await gather();\n\
                 println(s);\n\
                 var v: i64 = await pick(false);\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "data:x n=17\n7\n");
}

#[test]
fn await_genuine_suspension_via_yield_under_gc_stress() {
    // `yield_now()` returns Pending once (parking the executor) then Ready. In a
    // loop this forces the state machine to suspend, the executor to park and be
    // re-woken, and resumption to reload the loop's locals — three times.
    let src = "function counter(): Future<i64> async {\n\
                 var sum: i64 = 0;\n\
                 var i: i64 = 0;\n\
                 while i < 3 {\n\
                   var _ = await yield_now();\n\
                   sum = sum + i;\n\
                   i = i + 1;\n\
                 }\n\
                 sum\n\
               }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await counter();\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "3\n");
    // Native parity.
    let (nat, nerr, nok) = lang_build_run(src, &[]);
    assert!(nok, "native stderr: {nerr}");
    assert_eq!(nat, "3\n");
}

#[test]
fn await_inside_async_block() {
    // An async main awaits an inline `async { await … }` block — the
    // doc's primary launcher pattern.
    let src = "function tick(): Future<i64> async { var _ = await yield_now(); 7 }\n\
               function main(): Future<null> async {\n\
                 var r: i64 = await async {\n\
                   var a: i64 = await tick();\n\
                   var b: i64 = await tick();\n\
                   a + b + 100\n\
                 };\n\
                 println(r as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "114\n");
}

#[test]
fn async_spawn_drives_futures_on_workers() {
    // `spawn EXPR` (keyword) schedules a future on a worker and itself
    // evaluates to a `Future<T>` — awaiting it yields T directly (`docs/21`).
    let src = "function work(n: i64): Future<i64> async { var _ = await yield_now(); n * n }\n\
               function main(): Future<null> async {\n\
                 var h: Future<i64> = spawn work(6);\n\
                 var h2: Future<i64> = spawn work(7);\n\
                 var a: i64 = await h;\n\
                 var b: i64 = await h2;\n\
                 println(a as str);\n\
                 println(b as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "36\n49\n");
}

#[test]
fn for_await_over_async_iterator() {
    // `for await` drives an `AsyncIterator` whose `next_async` returns an
    // `async { … }` future that suspends (yield_now) and mutates captured `self`.
    let src = "struct Counter { current: i64, end: i64 }\n\
               extend Counter: AsyncIterator<i64> {\n\
                 function next_async(self): Future<Item<i64> | Done> {\n\
                   async {\n\
                     if self.current >= self.end { Done {} }\n\
                     else {\n\
                       var _ = await yield_now();\n\
                       var v: i64 = self.current;\n\
                       self.current = self.current + 1;\n\
                       Item { value: v }\n\
                     }\n\
                   }\n\
                 }\n\
               }\n\
               function sum_stream(c: Counter): Future<i64> async {\n\
                 var total: i64 = 0;\n\
                 for await n in c { total = total + n; }\n\
                 total\n\
               }\n\
               function main(): Future<null> async {\n\
                 var c = Counter { current: 0, end: 5 };\n\
                 var v: i64 = await sum_stream(c);\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang_env("run", src, &[("LANG_GC", "stress")]);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "10\n");
}

#[test]
fn async_sleep_completes_after_delay() {
    // `sleep(ms)` is a `Future<null>` driven by a timer thread that wakes the
    // executor; awaiting it suspends until the delay elapses.
    let src = "function delayed(x: i64): Future<i64> async { var _ = await sleep(5); x * 2 }\n\
               function main(): Future<null> async {\n\
                 var v: i64 = await delayed(21);\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "42\n");
}

#[test]
fn async_cancel_is_a_safe_noop() {
    // `.cancel()` on a compute-only future releases nothing and is repeatable.
    let src = "function task(): Future<i64> async { var _ = await yield_now(); 5 }\n\
               function main(): Future<null> async {\n\
                 var f: Future<i64> = task();\n\
                 f.cancel();\n\
                 f.cancel();\n\
                 var v: i64 = await task();\n\
                 println(v as str);\n\
               }";
    let (out, err, ok) = lang("run", src);
    assert!(ok, "stderr: {err}");
    assert_eq!(out, "5\n");
}

#[test]
fn check_rejects_forgotten_future() {
    // docs/21 §5: a Future created as a statement and discarded is an error.
    let src = "function answer(): Future<i64> async { 1 }\n\
               function main() { answer(); }";
    let (_, err, ok) = lang("check", src);
    assert!(!ok);
    assert!(err.contains("never used"), "stderr: {err}");
}
