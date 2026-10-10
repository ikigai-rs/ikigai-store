//! The measurement behind `src/limits.rs`'s bounds (ledger #915), kept so it can be re-run
//! when oxigraph moves: how deep each SPARQL shape goes on a 2 MiB thread before the
//! process aborts, in the parser (`MODES=parse`) and through evaluation (`MODES=run`).
//!
//!     cargo test --release --test sparql_stack_measure -- --ignored --nocapture
//!     SHAPES="parens calls" MODES=parse CAP=5000 cargo test --test sparql_stack_measure -- --ignored --nocapture
//!
//! Every probe runs in a child process — this binary re-executed — so an overflow aborts the
//! child and never the measurement. It calls oxigraph DIRECTLY, beneath this crate's bounds,
//! because what it measures is what those bounds have to hold back. A probe still running
//! after 20 s is reported as a timeout, not an abort: some shapes (nested collections, long
//! paths, long BGPs) get slow in the evaluator long before they get deep.
//!
//! Measured 2026-10-09, oxigraph 0.5.11, aarch64-apple-darwin (largest surviving n):
//!
//! | shape | parse, debug | parse, release | run, debug | run, release |
//! | --- | --- | --- | --- | --- |
//! | `(` in FILTER | 186 | 884 | 186 | 884 |
//! | `{` groups | 180 | 845 | 180 | 845 |
//! | `STR(` calls | 33 | 425 | 33 | 425 |
//! | `[ ]` bnodes | 329 | 605 | 100 | 605 |
//! | `( )` collections | 619 | 1678 | 51 | 939 (then > 20 s) |
//! | `!` prefix (parse error at the end, so `run` measures the parse) | 1383 | 5228 | 1383 | 5228 |
//! | `+1` chain | 1149 | 4091 | 46 | 3282 |
//! | `\|\|` chain | 18739 | 32912 | 299 | 2085 |
//! | `\|\|1` chain, 3 bytes a term | | 32912 | 299 | 2085 |
//! | `*1` chain | 1149 | 4091 | 46 | 3282 |
//! | `/:a` path | | 4366 | | 1877 (then > 20 s) |
//! | `/` path | 764 | 4366 | 100 | 1877 (then > 20 s) |
//! | `UNION` chain | 4807 | 16398 | 165 | 1132 |
//! | `OPTIONAL` list | 4806 | 16397 | 100 | 1250 |
//! | triple patterns | | | | 1878 (then > 20 s) |
//! | `FILTER` list | | | | 2086 |
//! | `BIND` list | | | | 1250 |
//! | `IN (…)` list | ≥ 400000 | | 826 | 8759 |
//! | `1 IN (1,1,…)`, 2 bytes a member | | | 825 | 8758 |
//! | `COALESCE(1,1,…)`, `CONCAT(…)` | | | ≥ 600000, flat | |
//! | `VALUES` | | | ≥ 600000, flat | ≥ 40000, flat |
//! | `INSERT DATA` | | | | ≥ 40000, flat |
//! | unary `-`, `+`, path `^` | ≥ 20000, no recursion | | | |
//!
//! Re-measured in a DEBUG build 2026-10-09 for ledger #1003 (`run, debug` for the chains and
//! lists; `IN` and the arithmetic chains also in release). Per byte, the costliest debug
//! shapes are `1 IN (1,1,…)` evaluated (~1,245 bytes of stack a byte: `IN` is one algebra node
//! however long, rewritten into one `||` level a member) and an arithmetic chain parsed
//! (~900); per algebra node, an arithmetic chain evaluated (~45 KiB). `src/limits.rs`'s
//! `DEBUG_STACK_PER_BYTE` and `DEBUG_STACK_PER_NODE` are sized from these.

use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use std::process::Command;

const ENV: &str = "IKIGAI_STORE_MEASURE";

fn shape(name: &str, n: usize) -> (bool, String) {
    let r = |s: &str, k: usize| s.repeat(k);
    match name {
        "parens" => (
            false,
            format!("SELECT * WHERE {{ FILTER({}1{}) }}", r("(", n), r(")", n)),
        ),
        "braces" => (false, format!("SELECT * WHERE {}{}", r("{", n), r("}", n))),
        "bnodes" => (
            false,
            format!(
                "SELECT * WHERE {{ ?s <urn:p> {}<urn:o>{} }}",
                r("[ <urn:p> ", n),
                r(" ]", n)
            ),
        ),
        "collections" => (
            false,
            format!("SELECT * WHERE {{ ?s <urn:p> {}{} }}", r("(", n), r(")", n)),
        ),
        "calls" => (
            false,
            format!(
                "SELECT * WHERE {{ FILTER({}1{}) }}",
                r("STR(", n),
                r(")", n)
            ),
        ),
        "not" => (
            false,
            format!("SELECT * WHERE {{ FILTER({}true) }}", r("!", n)),
        ),
        "neg" => (
            false,
            format!("SELECT * WHERE {{ FILTER({}1 > 0) }}", r("- ", n)),
        ),
        "plus" => (
            false,
            format!("SELECT * WHERE {{ FILTER(1{} > 0) }}", r("+1", n)),
        ),
        "or" => (
            false,
            format!("SELECT * WHERE {{ FILTER(false{}) }}", r("||false", n)),
        ),
        "path" => (
            false,
            format!("SELECT * WHERE {{ ?s <urn:p>{} ?o }}", r("/<urn:p>", n)),
        ),
        "alt" => (
            false,
            format!("SELECT * WHERE {{ ?s <urn:p>{} ?o }}", r("|<urn:p>", n)),
        ),
        "union" => (
            false,
            format!("SELECT * WHERE {{ {{}}{} }}", r(" UNION {}", n)),
        ),
        "optional" => (
            false,
            format!(
                "SELECT * WHERE {{ ?s ?p ?o {} }}",
                r("OPTIONAL { ?s ?p ?o } ", n)
            ),
        ),
        "u-parens" => (
            true,
            format!(
                "INSERT {{ <urn:s> <urn:p> <urn:o> }} WHERE {{ FILTER({}1{}) }}",
                r("(", n),
                r(")", n)
            ),
        ),
        "u-braces" => (
            true,
            format!(
                "INSERT {{ <urn:s> <urn:p> <urn:o> }} WHERE {}{}",
                r("{", n),
                r("}", n)
            ),
        ),
        "u-bnodes" => (
            true,
            format!(
                "INSERT DATA {{ <urn:s> <urn:p> {}<urn:o>{} }}",
                r("[ <urn:p> ", n),
                r(" ]", n)
            ),
        ),
        "u-collections" => (
            true,
            format!(
                "INSERT DATA {{ <urn:s> <urn:p> {}{} }}",
                r("(", n),
                r(")", n)
            ),
        ),
        "bgp" => (
            false,
            format!(
                "SELECT * WHERE {{ {} }}",
                (0..n)
                    .map(|i| format!("?s <urn:p> ?o{i} . "))
                    .collect::<String>()
            ),
        ),
        "values" => (
            false,
            format!(
                "SELECT * WHERE {{ VALUES ?i {{ {} }} ?i <urn:p> ?o }}",
                (0..n)
                    .map(|i| format!("<urn:iki:ledger:default:item:{i:026}> "))
                    .collect::<String>()
            ),
        ),
        "filters" => (
            false,
            format!("SELECT * WHERE {{ ?s ?p ?o {} }}", r("FILTER(true) ", n)),
        ),
        "binds" => (
            false,
            format!(
                "SELECT * WHERE {{ ?s ?p ?o {} }}",
                (0..n)
                    .map(|i| format!("BIND(1 AS ?b{i}) "))
                    .collect::<String>()
            ),
        ),
        "in" => (
            false,
            format!(
                "SELECT * WHERE {{ ?s ?p ?o FILTER(?o IN ({})) }}",
                (0..n)
                    .map(|i| format!("<urn:x{i}>"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        // The shortest IN member: `IN` costs one algebra node however long its list, but
        // the evaluator rewrites it into an `||` of `=`, one level a member (ledger #1003).
        "in1" => (
            false,
            format!(
                "SELECT * WHERE {{ FILTER(1 IN ({})) }}",
                vec!["1"; n.max(1)].join(",")
            ),
        ),
        "in-var" => (
            false,
            format!(
                "SELECT * WHERE {{ ?s ?p ?o FILTER(?o IN ({})) }}",
                vec!["?o"; n.max(1)].join(",")
            ),
        ),
        "coalesce" => (
            false,
            format!(
                "SELECT * WHERE {{ FILTER(COALESCE({}) > 0) }}",
                vec!["1"; n.max(1)].join(",")
            ),
        ),
        "concat" => (
            false,
            format!(
                "SELECT * WHERE {{ FILTER(CONCAT({}) != \"\") }}",
                vec!["\"a\""; n.max(1)].join(",")
            ),
        ),
        "values1" => (
            false,
            format!("SELECT * WHERE {{ VALUES ?x {{ {} }} }}", r("1 ", n)),
        ),
        "eqor" => (
            false,
            format!(
                "SELECT * WHERE {{ ?s ?p ?o FILTER({}) }}",
                (0..n)
                    .map(|i| format!("?o = <urn:x{i}>"))
                    .collect::<Vec<_>>()
                    .join(" || ")
            ),
        ),
        "insert-data" => (
            true,
            format!(
                "INSERT DATA {{ {} }}",
                (0..n)
                    .map(|i| format!("<urn:s{i}> <urn:p> <urn:o> . "))
                    .collect::<String>()
            ),
        ),
        "or1" => (
            false,
            format!("SELECT * WHERE {{ FILTER(1{}) }}", r("||1", n)),
        ),
        "and1" => (
            false,
            format!("SELECT * WHERE {{ FILTER(1{}) }}", r("&&1", n)),
        ),
        "mul" => (
            false,
            format!("SELECT * WHERE {{ FILTER(1{} > 0) }}", r("*1", n)),
        ),
        "pname-path" => (
            false,
            format!(
                "PREFIX : <urn:> SELECT * WHERE {{ ?s :a{} ?o }}",
                r("/:a", n)
            ),
        ),
        "minus" => (
            false,
            format!("SELECT * WHERE {{ FILTER({}1 > 0) }}", r("-", n)),
        ),
        "uplus" => (
            false,
            format!("SELECT * WHERE {{ FILTER({}1 > 0) }}", r("+", n)),
        ),
        "inverse" => (
            false,
            format!("SELECT * WHERE {{ ?s {}<urn:p> ?o }}", r("^", n)),
        ),
        "not-paren" => (
            false,
            format!(
                "SELECT * WHERE {{ FILTER({}true{}) }}",
                r("!(", n),
                r(")", n)
            ),
        ),
        other => panic!("unknown shape {other}"),
    }
}

/// Run one probe in-process: `parse` stops after parsing; `run` also evaluates and drains.
fn probe(mode: &str, name: &str, n: usize) -> String {
    let (update, text) = shape(name, n);
    let store = Store::new().unwrap();
    store
        .insert(QuadRef::new(
            NamedNodeRef::new("urn:s").unwrap(),
            NamedNodeRef::new("urn:p").unwrap(),
            NamedNodeRef::new("urn:o").unwrap(),
            GraphNameRef::DefaultGraph,
        ))
        .unwrap();
    if update {
        match SparqlEvaluator::new().parse_update(&text) {
            Err(e) => format!(
                "parse-error {}",
                e.to_string().chars().take(60).collect::<String>()
            ),
            Ok(p) if mode == "parse" => {
                drop(p);
                "parsed".into()
            }
            Ok(p) => match p.on_store(&store).execute() {
                Ok(()) => "ran".into(),
                Err(e) => format!(
                    "eval-error {}",
                    e.to_string().chars().take(60).collect::<String>()
                ),
            },
        }
    } else {
        match SparqlEvaluator::new().parse_query(&text) {
            Err(e) => format!(
                "parse-error {}",
                e.to_string().chars().take(60).collect::<String>()
            ),
            Ok(p) if mode == "parse" => {
                drop(p);
                "parsed".into()
            }
            Ok(p) => match p.on_store(&store).execute() {
                Ok(QueryResults::Solutions(s)) => format!("ran {}", s.count()),
                Ok(_) => "ran".into(),
                Err(e) => format!(
                    "eval-error {}",
                    e.to_string().chars().take(60).collect::<String>()
                ),
            },
        }
    }
}

#[test]
fn measure_child() {
    let Ok(spec) = std::env::var(ENV) else { return };
    let mut it = spec.split(':');
    let (mode, name, n) = (
        it.next().unwrap().to_string(),
        it.next().unwrap().to_string(),
        it.next().unwrap().parse::<usize>().unwrap(),
    );
    let out = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || probe(&mode, &name, n))
        .unwrap()
        .join()
        .unwrap();
    println!("OUTCOME {out}");
    std::process::exit(0);
}

fn child(mode: &str, name: &str, n: usize) -> Option<String> {
    let mut proc_ = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "measure_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ENV, format!("{mode}:{name}:{n}"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if proc_.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = proc_.kill();
            let _ = proc_.wait();
            return Some("TIMEOUT (no abort within 20 s)".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = proc_.wait_with_output().unwrap();
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let at = stdout
        .find("OUTCOME ")
        .expect("a surviving child reports its outcome");
    Some(stdout[at + 8..].lines().next().unwrap_or("").to_string())
}

#[test]
#[ignore]
fn measure_overflow_depths() {
    let shapes = std::env::var("SHAPES").unwrap_or_else(|_| {
        "parens braces bnodes collections calls not neg plus or path alt union optional u-parens u-braces u-bnodes u-collections".into()
    });
    let modes = std::env::var("MODES").unwrap_or_else(|_| "parse run".into());
    let cap: usize = std::env::var("CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);
    for mode in modes.split_whitespace() {
        for name in shapes.split_whitespace() {
            // Largest n that survives, by doubling then bisecting.
            let mut lo = 0usize;
            let mut hi = 16usize;
            let mut last_ok = String::new();
            while let Some(o) = child(mode, name, hi) {
                lo = hi;
                last_ok = o;
                if hi >= cap {
                    break;
                }
                hi = (hi * 2).min(cap);
            }
            if lo < cap || child(mode, name, cap).is_none() {
                while hi - lo > 1 {
                    let mid = (lo + hi) / 2;
                    match child(mode, name, mid) {
                        Some(o) => {
                            lo = mid;
                            last_ok = o;
                        }
                        None => hi = mid,
                    }
                }
                let bytes = shape(name, hi).1.len();
                println!("MEASURE mode={mode} shape={name} survives={lo} aborts={hi} abort_bytes={bytes} last_ok=[{last_ok}]");
            } else {
                println!("MEASURE mode={mode} shape={name} survives>={cap} (no abort) last_ok=[{last_ok}]");
            }
        }
    }
}
