//! The measurement behind `src/limits.rs`'s TIME budget (ledger #964), kept so it can be
//! re-run when oxigraph moves: how long each SPARQL shape takes to evaluate, and — the
//! question the budget stands on — **whether oxigraph's cancellation token actually stops
//! it**, and how long after it fires.
//!
//!     cargo test --release --test sparql_time_measure -- --ignored --nocapture
//!     SHAPES="or1 path" SIZES="1000 2000" CANCEL_MS=500 KILL_S=60 \
//!         cargo test --release --test sparql_time_measure -- --ignored --nocapture
//!
//! Every probe runs in a child process — this binary re-executed — with a HARD KILL after
//! `KILL_S` seconds (default 30), so a probe that the token cannot stop costs a bounded
//! amount of the machine and is reported as such rather than hanging the measurement. It
//! calls oxigraph DIRECTLY, beneath this crate's bounds, on a stack sized the way
//! `limits::on_sparql_stack` sizes it.
//!
//! Each line reports the phases separately because they differ in what the token reaches:
//! `parse` (spargebra), `exec` (`execute()`: the optimizer and plan building, then the
//! first step of evaluation), `drain` (iterating the results, which is where oxigraph
//! evaluates lazily) — and, with `CANCEL_MS` set, `stopped`: how long after the token
//! fired the work actually returned.

use oxigraph::model::{GraphNameRef, NamedNodeRef, QuadRef};
use oxigraph::sparql::{CancellationToken, QueryResults, SparqlEvaluator};
use oxigraph::store::Store;
use std::process::Command;
use std::time::{Duration, Instant};

const ENV: &str = "IKIGAI_STORE_TIME_MEASURE";

/// `(is an update, text)` for a shape at size `n`.
fn shape(name: &str, n: usize) -> (bool, String) {
    let r = |s: &str, k: usize| s.repeat(k);
    match name {
        // The two shapes ledger #964 names.
        "or1" => (
            false,
            format!("SELECT * WHERE {{ FILTER(1{}) }}", r("||1", n)),
        ),
        "path" => (
            false,
            format!(
                "PREFIX : <urn:> SELECT * WHERE {{ ?s :p{} ?o }}",
                r("/:p", n)
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
        // The classic: a cross product of `n` unconstrained patterns, |data|^n rows, in a
        // few dozen bytes. Exponential in n, so n stays small.
        "cross" => (
            false,
            format!(
                "SELECT (COUNT(*) AS ?c) WHERE {{ {} }}",
                (0..n)
                    .map(|i| format!("?s{i} ?p{i} ?o{i} . "))
                    .collect::<String>()
            ),
        ),
        // The same with every row returned rather than counted: the cost is in the rows.
        "cross-rows" => (
            false,
            format!(
                "SELECT * WHERE {{ {} }}",
                (0..n)
                    .map(|i| format!("?s{i} ?p{i} ?o{i} . "))
                    .collect::<String>()
            ),
        ),
        // The same through a path closure over a dense graph.
        "closure" => (
            false,
            format!(
                "SELECT (COUNT(*) AS ?c) WHERE {{ ?a (<urn:p>|^<urn:p>)* ?b . {} }}",
                (0..n)
                    .map(|i| format!("?x{i} (<urn:p>|^<urn:p>)* ?y{i} . "))
                    .collect::<String>()
            ),
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
        "filters" => (
            false,
            format!("SELECT * WHERE {{ ?s ?p ?o {} }}", r("FILTER(true) ", n)),
        ),
        "collections" => (
            false,
            format!("SELECT * WHERE {{ ?s <urn:p> {}{} }}", r("(", n), r(")", n)),
        ),
        // An update whose WHERE is the cross product: what a cancelled update leaves.
        "u-cross" => (
            true,
            format!(
                "INSERT {{ ?s0 <urn:copied> ?o0 }} WHERE {{ {} }}",
                (0..n)
                    .map(|i| format!("?s{i} ?p{i} ?o{i} . "))
                    .collect::<String>()
            ),
        ),
        "u-or1" => (
            true,
            format!(
                "INSERT {{ <urn:s> <urn:p> <urn:o> }} WHERE {{ FILTER(1{}) }}",
                r("||1", n)
            ),
        ),
        other => panic!("unknown shape {other}"),
    }
}

/// A small dense graph: `DATA` subjects in a ring and a fan, every edge `urn:p`.
fn store() -> Store {
    let store = Store::new().unwrap();
    let p = NamedNodeRef::new("urn:p").unwrap();
    let data: usize = std::env::var("DATA")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let iri = |i: usize| format!("urn:n{i}");
    for i in 0..data {
        for j in [i + 1, i * 7 + 3] {
            let (s, o) = (iri(i), iri(j % data));
            store
                .insert(QuadRef::new(
                    NamedNodeRef::new(&s).unwrap(),
                    p,
                    NamedNodeRef::new(&o).unwrap(),
                    GraphNameRef::DefaultGraph,
                ))
                .unwrap();
        }
    }
    store
}

fn ms(d: Duration) -> u128 {
    d.as_millis()
}

/// Progress on stdout as it happens, so a probe the parent has to kill still says which
/// phase it was in.
fn phase(name: &str, took: Duration) {
    use std::io::Write;
    println!("PHASE {name} {}ms", ms(took));
    let _ = std::io::stdout().flush();
}

/// Run one probe in-process and describe what happened, phase by phase.
fn probe(name: &str, n: usize, cancel_ms: Option<u64>) -> String {
    let (update, text) = shape(name, n);
    let store = store();
    let before = store.len().unwrap();
    let token = CancellationToken::new();
    let fired = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
    if let Some(after) = cancel_ms {
        let (token, fired) = (token.clone(), fired.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(after));
            *fired.lock().unwrap() = Some(Instant::now());
            token.cancel();
        });
    }
    let stopped = |fired: &std::sync::Mutex<Option<Instant>>| match *fired.lock().unwrap() {
        Some(at) => format!(" stopped={}ms-after-cancel", ms(at.elapsed())),
        None => String::new(),
    };
    let evaluator = SparqlEvaluator::new().with_cancellation_token(token);
    let t = Instant::now();
    if update {
        let prepared = match evaluator.parse_update(&text) {
            Ok(p) => p,
            Err(e) => return format!("parse-error {e}"),
        };
        let parse = t.elapsed();
        let t = Instant::now();
        let outcome = match prepared.on_store(&store).execute() {
            Ok(()) => "ok".to_string(),
            Err(e) => format!(
                "err[{}]",
                e.to_string().chars().take(50).collect::<String>()
            ),
        };
        format!(
            "bytes={} parse={}ms exec={}ms {outcome} quads {before}->{}{}",
            text.len(),
            ms(parse),
            ms(t.elapsed()),
            store.len().unwrap(),
            stopped(&fired)
        )
    } else {
        let prepared = match evaluator.parse_query(&text) {
            Ok(p) => p,
            Err(e) => return format!("parse-error {e}"),
        };
        let parse = t.elapsed();
        phase("parsed", parse);
        let t = Instant::now();
        let results = prepared.on_store(&store).execute();
        let exec = t.elapsed();
        phase("executed", exec);
        let t = Instant::now();
        let outcome = match results {
            Err(e) => format!(
                "exec-err[{}]",
                e.to_string().chars().take(50).collect::<String>()
            ),
            Ok(QueryResults::Solutions(solutions)) => {
                let (mut rows, mut err) = (0usize, None);
                for s in solutions {
                    match s {
                        Ok(_) => rows += 1,
                        Err(e) => {
                            err = Some(e.to_string());
                            break;
                        }
                    }
                }
                match err {
                    Some(e) => format!(
                        "rows={rows} err[{}]",
                        e.chars().take(50).collect::<String>()
                    ),
                    None => format!("rows={rows}"),
                }
            }
            Ok(_) => "ok".into(),
        };
        format!(
            "bytes={} parse={}ms exec={}ms drain={}ms {outcome}{}",
            text.len(),
            ms(parse),
            ms(exec),
            ms(t.elapsed()),
            stopped(&fired)
        )
    }
}

#[test]
fn time_child() {
    let Ok(spec) = std::env::var(ENV) else { return };
    let mut it = spec.split(':');
    let name = it.next().unwrap().to_string();
    let n = it.next().unwrap().parse::<usize>().unwrap();
    let cancel = it.next().and_then(|s| s.parse::<u64>().ok());
    let text_len = shape(&name, n).1.len();
    // The stack `limits::on_sparql_stack` would give this text.
    let stack = ikigai_store::limits::STACK_BASE + text_len * ikigai_store::limits::STACK_PER_BYTE;
    let out = std::thread::Builder::new()
        .stack_size(stack)
        .spawn(move || probe(&name, n, cancel))
        .unwrap()
        .join()
        .unwrap();
    println!("OUTCOME {out}");
    std::process::exit(0);
}

fn child(name: &str, n: usize, cancel: Option<u64>, kill: Duration) -> String {
    let mut proc_ = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "time_child", "--nocapture", "--test-threads=1"])
        .env(
            ENV,
            format!(
                "{name}:{n}:{}",
                cancel.map(|c| c.to_string()).unwrap_or_default()
            ),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if proc_.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > kill {
            let _ = proc_.kill();
            let out = proc_.wait_with_output().unwrap();
            let phases: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix("PHASE ").map(str::to_string))
                .collect();
            return format!(
                "KILLED after {} s (still running; done: [{}])",
                kill.as_secs(),
                phases.join(", ")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = proc_.wait_with_output().unwrap();
    let wall = start.elapsed();
    if !out.status.success() {
        return format!("ABORTED ({})", out.status);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    match stdout.find("OUTCOME ") {
        Some(at) => format!(
            "{} wall={}ms",
            stdout[at + 8..].lines().next().unwrap_or(""),
            ms(wall)
        ),
        None => "no outcome".into(),
    }
}

#[test]
#[ignore]
fn measure_evaluation_time() {
    let shapes = std::env::var("SHAPES").unwrap_or_else(|_| "or1 path bgp cross".into());
    let sizes = std::env::var("SIZES").unwrap_or_else(|_| "500 1000 2000 4000".into());
    let cancel = std::env::var("CANCEL_MS").ok().and_then(|s| s.parse().ok());
    let kill = Duration::from_secs(
        std::env::var("KILL_S")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
    );
    for name in shapes.split_whitespace() {
        for n in sizes.split_whitespace() {
            let n: usize = n.parse().unwrap();
            println!(
                "MEASURE shape={name} n={n} cancel={cancel:?} {}",
                child(name, n, cancel, kill)
            );
        }
    }
}

/// The other half of the evidence: how long the slowest LEGITIMATE queries take, over a
/// real dataset. `DATASET` names an N-Quads file (a gonk backup, gunzipped); nothing from it
/// is printed but timings and row counts.
///
///     DATASET=/path/gonk-store.nq cargo test --release --test sparql_time_measure \
///         measure_legitimate -- --ignored --nocapture
#[test]
#[ignore]
fn measure_legitimate() {
    use oxigraph::io::RdfFormat;
    use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
    let Ok(path) = std::env::var("DATASET") else {
        println!("set DATASET to an N-Quads file");
        return;
    };
    // `ROCKSDB_DIR` (with `--features persistent`) measures the durable backing instead.
    let store = match std::env::var("ROCKSDB_DIR") {
        #[cfg(feature = "persistent")]
        Ok(dir) => Store::open(dir).unwrap(),
        _ => Store::new().unwrap(),
    };
    let t = Instant::now();
    store
        .load_from_reader(
            RdfFormat::NQuads,
            std::io::BufReader::new(std::fs::File::open(&path).unwrap()),
        )
        .unwrap();
    println!(
        "LOADED {} quads in {}ms",
        store.len().unwrap(),
        ms(t.elapsed())
    );
    let ledger = "urn:iki:ledger:graph:default";
    // Every ledger item IRI, for the VALUES-heavy queries the ledger issues.
    let items: Vec<String> = match SparqlEvaluator::new()
        .parse_query(&format!(
            "SELECT DISTINCT ?i WHERE {{ GRAPH <{ledger}> {{ ?i <https://ikigai-rs.dev/ns/ledger#number> ?n }} }}"
        ))
        .unwrap()
        .on_store(&store)
        .execute()
        .unwrap()
    {
        QueryResults::Solutions(s) => s
            .map(|s| s.unwrap().get("i").unwrap().to_string())
            .collect(),
        _ => unreachable!(),
    };
    let values = items.join(" ");
    let queries: Vec<(&str, String)> = vec![
        (
            "gonk backup (every graph, ORDER BY)",
            "SELECT ?g ?s ?p ?o WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } } ORDER BY ?g ?s ?p ?o".into(),
        ),
        (
            "per-graph counts",
            "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g".into(),
        ),
        (
            "ledger multivalued fill, every item",
            format!(
                "SELECT ?item ?p ?o WHERE {{ GRAPH <{ledger}> {{ VALUES ?item {{ {values} }} ?item ?p ?o }} }}"
            ),
        ),
        (
            "ledger comments, every item, ORDER BY",
            format!(
                "SELECT ?item ?c ?b WHERE {{ GRAPH <{ledger}> {{ VALUES ?item {{ {values} }} ?c ?on ?item ; ?bp ?b }} }} ORDER BY ?c"
            ),
        ),
        (
            "whole browse graph, unordered",
            "SELECT * WHERE { GRAPH <urn:iki:browse:graph:default> { ?s ?p ?o } }".into(),
        ),
    ];
    println!("ITEMS {}", items.len());
    for (name, query) in queries {
        let t = Instant::now();
        let results = SparqlEvaluator::new()
            .parse_query(&query)
            .unwrap()
            .on_store(&store)
            .execute()
            .unwrap();
        let QueryResults::Solutions(solutions) = results else {
            unreachable!()
        };
        let mut serializer = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
            .serialize_solutions_to_writer(Vec::new(), solutions.variables().to_vec())
            .unwrap();
        let mut rows = 0usize;
        for s in solutions {
            serializer.serialize(&s.unwrap()).unwrap();
            rows += 1;
        }
        let bytes = serializer.finish().unwrap().len();
        println!(
            "LEGIT {name}: {}ms, {rows} rows, {bytes} bytes, query {} bytes",
            ms(t.elapsed()),
            query.len()
        );
    }
}
