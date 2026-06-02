//! The Otter Fusion end-to-end test suite runner.
//!
//! Discovers every `.otter` case under the repository's `tests/cases/` tree,
//! runs each through the real `otter_fusion` binary, and checks its outcome
//! against the expectations declared inside the file (see `framework`).
//!
//! Run it directly with a timing report:
//!
//! ```text
//! cargo test -p cli --test suite -- --nocapture
//! ```
//!
//! Update expected stdout for passing `run` cases:
//!
//! ```text
//! OTTER_TEST_BLESS=1 cargo test -p cli --test suite -- --nocapture
//! ```

mod framework;

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use framework::{Case, Kind, Status};

/// The `tests/cases/` directory at the repository root.
fn cases_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("cases")
}

/// The compiled `otter_fusion` binary under test.
fn otter_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_otter_fusion"))
}

#[test]
fn end_to_end_suite() {
    let root = cases_root();
    assert!(
        root.is_dir(),
        "cases directory not found at {} — the suite corpus is missing",
        root.display()
    );
    let bless = std::env::var("OTTER_TEST_BLESS").is_ok();
    let otter = otter_bin();

    let paths = framework::discover(&root);
    assert!(
        !paths.is_empty(),
        "no test cases found under {}",
        root.display()
    );

    // Parse every case up front so malformed directives fail loudly.
    let mut cases: Vec<Case> = Vec::new();
    for p in &paths {
        let src = std::fs::read_to_string(p).expect("read case");
        match Case::parse(p, &root, &src) {
            Ok(c) => cases.push(c),
            Err(e) => panic!("invalid test directives in {}: {e}", p.display()),
        }
    }

    // Cases marked `//@ serial` (OS-thread spawners) run alone afterwards to
    // avoid the runtime's cross-process thread-contention fragility; the rest
    // run across a small pool. Each case shells out to an isolated
    // `otter_fusion` process, so non-serial cases share no in-process state.
    let parallel_idx: Vec<usize> = (0..cases.len()).filter(|&i| !cases[i].serial).collect();
    let serial_idx: Vec<usize> = (0..cases.len()).filter(|&i| cases[i].serial).collect();

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);
    let (tx, rx) = mpsc::channel();
    let cases = std::sync::Arc::new(cases);
    let pidx = std::sync::Arc::new(parallel_idx);
    let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let cases = cases.clone();
            let pidx = pidx.clone();
            let next = next.clone();
            let otter = otter.clone();
            scope.spawn(move || {
                loop {
                    let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if k >= pidx.len() {
                        break;
                    }
                    let i = pidx[k];
                    let outcome = framework::run_case(&otter, &cases[i]);
                    tx.send((i, outcome)).unwrap();
                }
            });
        }
        drop(tx);
    });

    let mut outcomes: Vec<Option<framework::Outcome>> = (0..cases.len()).map(|_| None).collect();
    for (i, o) in rx {
        outcomes[i] = Some(o);
    }
    // Serial cases: one at a time, nothing else running.
    for i in serial_idx {
        outcomes[i] = Some(framework::run_case(&otter, &cases[i]));
    }
    let outcomes: Vec<framework::Outcome> = outcomes.into_iter().map(|o| o.unwrap()).collect();

    // Bless: rewrite passing `run` cases' expected stdout to actual output.
    if bless {
        let mut blessed = 0;
        for (case, o) in cases.iter().zip(&outcomes) {
            // Never overwrite a known-bug case: its `//~` documents the DESIRED
            // (not-yet-achieved) output, which blessing would clobber.
            if case.kind == Kind::Run && case.known_bug.is_none() {
                if let Err(e) = framework::bless(case, &o.actual_stdout) {
                    eprintln!("bless failed for {}: {e}", case.name);
                } else {
                    blessed += 1;
                }
            }
        }
        println!("blessed {blessed} run case(s); re-run the suite to verify.");
        return;
    }

    report(&cases, &outcomes);

    // The suite fails on genuine failures (Fail) and on known-bugs that
    // unexpectedly started passing (XPass) — both mean the catalog is stale.
    let failed: Vec<&framework::Outcome> = outcomes.iter().filter(|o| !o.ok()).collect();
    if !failed.is_empty() {
        let mut msg = format!(
            "\n{} of {} test case(s) FAILED:\n",
            failed.len(),
            outcomes.len()
        );
        for o in &failed {
            let tag = match o.status {
                Status::XPass => "XPASS (known-bug now passes — remove marker)",
                _ => "FAIL",
            };
            msg.push_str(&format!(
                "\n=== {} [{tag}] ===\n{}\n",
                o.name,
                o.failure.as_deref().unwrap_or("")
            ));
        }
        panic!("{msg}");
    }
}

/// Print a per-category status + timing report to stdout (visible under
/// `--nocapture`), including the catalog of known bugs (XFAIL).
fn report(cases: &[Case], outcomes: &[framework::Outcome]) {
    let total = outcomes.len();
    let count = |s: Status| outcomes.iter().filter(|o| o.status == s).count();
    let passed = count(Status::Pass);
    let failed = count(Status::Fail);
    let xfail = count(Status::XFail);
    let xpass = count(Status::XPass);

    println!("\n┌─ Otter Fusion end-to-end suite ─────────────────────────────");
    println!(
        "│ {total} cases  ·  {passed} pass  ·  {failed} fail  ·  {xfail} known-bug (XFAIL)  ·  {xpass} XPASS"
    );

    // The known-bug catalog: the documented, still-present gaps in the language.
    if xfail > 0 || xpass > 0 {
        println!("├─ known bugs / unimplemented (spec says these should work) ──");
        for o in outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::XFail | Status::XPass))
        {
            let mark = if o.status == Status::XPass {
                "✗ now-passing"
            } else {
                "•"
            };
            let note = o
                .failure
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("");
            println!("│ {mark} {:<34} {note}", o.name);
        }
    }
    println!("├─ timing (pure execution, excludes compilation) ─────────────");

    // Group timings by top-level category for a readable summary.
    let mut by_cat: std::collections::BTreeMap<String, (usize, Duration)> = Default::default();
    let mut slowest: Vec<(&str, Duration)> = Vec::new();
    for (c, o) in cases.iter().zip(outcomes) {
        let cat = c.name.split('/').next().unwrap_or("").to_string();
        if let Some(t) = o.exec_time {
            let e = by_cat.entry(cat).or_insert((0, Duration::ZERO));
            e.0 += 1;
            e.1 += t;
            slowest.push((&o.name, t));
        }
    }
    for (cat, (n, sum)) in &by_cat {
        let avg = *sum / (*n as u32).max(1);
        println!(
            "│ {:<22} {:>4} cases   Σ {:>10}   avg {:>10}",
            cat,
            n,
            framework::fmt_duration(*sum),
            framework::fmt_duration(avg)
        );
    }
    slowest.sort_by(|a, b| b.1.cmp(&a.1));
    if !slowest.is_empty() {
        println!("├─ slowest cases ─────────────────────────────────────────────");
        for (name, t) in slowest.iter().take(5) {
            println!("│ {:>10}   {}", framework::fmt_duration(*t), name);
        }
    }
    println!("└─────────────────────────────────────────────────────────────\n");
}
