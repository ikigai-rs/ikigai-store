//! What a scoped write that observes its WHOLE graph costs, as the graph grows (ledger #146).
//!
//! `urn:iki:store:graph-update` evaluates an update against a private store holding the part of
//! the graph the update can observe (`src/confine.rs`). Since ledger #1132 that part is a quad
//! or two for an update whose patterns name constants, but an update that observes the graph as
//! a whole (`CLEAR`, `DROP`, a pattern with no constant, a path that can be zero-length) copies
//! every quad of it: into the private store, into the "before" set, and, after the update ran,
//! into the "after" set. This measures that copy, so the bound on it is a number someone
//! measured rather than a guess.
//!
//! Time is printed here; memory is read from outside, one size per run, with macOS's
//! `/usr/bin/time -l` (its "maximum resident set size"):
//!
//! ```text
//! cargo test --release --test scoped_copy_measure --no-run
//! QUADS=1000000 /usr/bin/time -l target/release/deps/scoped_copy_measure-<hash> \
//!     --ignored --nocapture
//! ```
//!
//! `QUADS` (default `100000,1000000`) is a comma-separated list of graph sizes, and `BOUND`
//! sets the store's copy bound for the run (default: high enough that nothing is refused).

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::sync::Arc;
use std::time::Instant;

const GRAPH: &str = "urn:x:copy-cost:g";

/// An update whose only pattern has no constant, so its footprint is the whole graph, and
/// whose `FILTER (false)` makes it write nothing: the copy is all it costs.
const WHOLE: &str = "INSERT { GRAPH <urn:x:copy-cost:g> { <urn:x:a> <urn:x:b> <urn:x:c> } } \
                     WHERE { GRAPH <urn:x:copy-cost:g> { ?s ?p ?o } FILTER (false) }";

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_a_whole_graph_scoped_write() {
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or(default.to_string());
    for quads in var("QUADS", "100000,1000000").split(',') {
        let quads: usize = quads.trim().parse().unwrap();
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        let bound: usize = var("BOUND", &format!("{}", quads + 1)).parse().unwrap();
        let store = store.with_max_scoped_copy_quads(bound);
        let graph = GraphName::from(NamedNode::new(GRAPH).unwrap());
        let started = Instant::now();
        handle
            .extend((0..quads).map(|i| {
                Quad::new(
                    NamedNode::new(format!("urn:x:copy-cost:s:{}", i / 8)).unwrap(),
                    NamedNode::new(format!("urn:x:copy-cost:p:{}", i % 8)).unwrap(),
                    Literal::new_simple_literal(format!("a literal of ordinary length, {i}")),
                    graph.clone(),
                )
            }))
            .unwrap();
        let loaded = started.elapsed();
        let kernel = Kernel::with_meta_renderer(
            Arc::new(space(store)),
            Arc::new(ikigai_vocab::TurtleRenderer),
        );
        let started = Instant::now();
        let answer = block_on(
            kernel.issue(
                Request::new(
                    Verb::Sink,
                    Iri::parse("urn:iki:store:graph-update").unwrap(),
                )
                .with_arg("content", ArgRef::Inline(WHOLE.as_bytes().to_vec()))
                .with_arg("graph", ArgRef::Inline(GRAPH.as_bytes().to_vec())),
                &Capability::root(),
            ),
        );
        let took = started.elapsed();
        println!(
            "{} build, memory, {quads} quads (loaded in {loaded:?}): whole-graph scoped write \
             {took:?}, {:.2} µs per quad: {}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            took.as_secs_f64() * 1e6 / quads.max(1) as f64,
            match answer {
                Ok(rep) => String::from_utf8(rep.bytes).unwrap().trim().to_string(),
                Err(e) => format!("refused: {e}"),
            }
        );
    }
}
