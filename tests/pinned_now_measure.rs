//! The measurement behind ledger #4286: how long a repeated `NOW()` read under a pinned
//! temporal corridor takes, kept so it can be re-run.
//!
//!     cargo test --release --test pinned_now_measure -- --ignored --nocapture
//!     READS=5000 TRIPLES=50000 cargo test --release --test pinned_now_measure -- --ignored --nocapture
//!
//! Each shape is read once to warm up, then `READS` times (default 2,000) in the same
//! corridor through `urn:iki:store:select`, and the line reports the median and mean per
//! read. Before 0.7.1 every repeat was evaluated again; since, every repeat is served from
//! the kernel cache. The file uses only APIs both versions have, so the same file measures
//! main and the branch.

use futures::executor::block_on;
use ikigai_core::{
    ArgRef, Capability, EndpointSpace, FixedClock, Iri, Kernel, Request, Scope, Space, Verb,
};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn select(query: &str) -> Request {
    Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()))
}

#[test]
#[ignore = "a measurement, not a check: run it with --release --ignored --nocapture"]
fn a_repeated_now_read_under_a_pinned_corridor() {
    let reads = env("READS", 2_000);
    let triples = env("TRIPLES", 10_000);
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let root = Capability::root();
    let data: String = (0..triples)
        .map(|i| format!("<urn:s:{i}> <urn:p> {i} .\n"))
        .collect();
    let load = Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
        "content",
        ArgRef::Inline(format!("INSERT DATA {{ {data} }}").into_bytes()),
    );
    block_on(kernel.issue(load, &root)).unwrap();
    let corridor = Scope::empty().with_named_at(
        Iri::parse("urn:example:corridor:measure").unwrap(),
        Arc::new(EndpointSpace::new()) as Arc<dyn Space>,
        Arc::new(FixedClock::at(1_000_000_000_000)),
    );

    for (name, query) in [
        ("now-only", "SELECT (NOW() AS ?t) WHERE {}".to_string()),
        (
            "now+count",
            "SELECT (NOW() AS ?t) (COUNT(*) AS ?n) WHERE { ?s ?p ?o }".to_string(),
        ),
        (
            "now+filter",
            "SELECT ?s WHERE { ?s <urn:p> ?o FILTER(?o < 100 && NOW() > \"2000-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>) }".to_string(),
        ),
    ] {
        let request = select(&query);
        block_on(kernel.issue_in(request.clone(), &root, corridor.clone())).unwrap();
        let cached = kernel.is_cached_in(&request, &root, &corridor);
        let mut times: Vec<Duration> = (0..reads)
            .map(|_| {
                let start = Instant::now();
                block_on(kernel.issue_in(request.clone(), &root, corridor.clone())).unwrap();
                start.elapsed()
            })
            .collect();
        times.sort();
        let mean = times.iter().sum::<Duration>() / reads as u32;
        println!(
            "{name:<10} triples={triples} reads={reads} cached={cached} median={:?} mean={mean:?}",
            times[reads / 2]
        );
    }
}
