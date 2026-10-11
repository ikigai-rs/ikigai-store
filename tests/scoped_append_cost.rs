//! What one ledger append costs through `urn:iki:store:graph-update`, as the graph grows
//! (ledger #1132).
//!
//! The scoped door evaluates an update against a private store holding only the target graph,
//! so nothing outside the graph can be read or written (`src/confine.rs`). Until ledger #1132
//! that private store was a copy of the WHOLE graph, made on every write, so each append cost
//! in proportion to everything appended before it. Now it holds the graph's registration and
//! only the quads the update's patterns can match (its "footprint"), so an append that names
//! its subject and predicate costs the same at 3,000 items as at 100.
//!
//! The update below is `ikigai-ledger`'s keyed append, verbatim in shape: a counter read in a
//! sub-select, a `FILTER NOT EXISTS` guarding the key, and the item's quads in the template.
//!
//! The pin is [`an_append_costs_the_same_at_three_thousand_items_as_at_a_hundred`]: generous
//! on purpose (the fixed code measures about 1x), so a slow runner has to be slow on every
//! append of the large graph to fail it. The deterministic pin of the mechanism is
//! `confine::tests`, which counts the quads an append copies.
//!
//! The measurement, kept so it can be re-run:
//!
//! ```text
//! cargo test --features persistent --test scoped_append_cost -- --ignored --nocapture
//! ITEMS=100,750,3000 APPENDS=20 cargo test --release --features persistent \
//!     --test scoped_append_cost -- --ignored --nocapture
//! ```

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, Store};
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GRAPH: &str = "urn:x:append-cost:ledger";
const NS: &str = "urn:x:append-cost:";

/// The most the median append at 3,000 items may take, as a multiple of the median at 100.
/// The fixed code measures about 1x; the whole-graph copy measured 9x to 26x.
const MAX_RATIO: f64 = 3.0;

fn kernel(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn issue(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, &str)]) -> String {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    let answer = block_on(kernel.issue(request, &Capability::root()))
        .unwrap_or_else(|e| panic!("{iri}: {e}"));
    String::from_utf8(answer.bytes).unwrap()
}

fn node(local: &str) -> NamedNode {
    NamedNode::new(format!("{NS}{local}")).unwrap()
}

/// `items` ledger-shaped items, and the counter at `items`, written straight to the store in
/// one transaction: the graph an append lands in, without paying for the appends that built it.
fn preload(handle: &Store, items: usize) {
    let graph = GraphName::from(NamedNode::new(GRAPH).unwrap());
    let quad = |s: &NamedNode, p: &str, o: oxigraph::model::Term| {
        Quad::new(s.clone(), node(p), o, graph.clone())
    };
    let mut quads = Vec::new();
    for i in 1..=items {
        let item = node(&format!("item:{i:020}"));
        quads.push(quad(&item, "type", node("Item").into()));
        quads.push(quad(&item, "number", Literal::from(i as i64).into()));
        quads.push(quad(
            &item,
            "title",
            Literal::new_simple_literal(format!("the title of item number {i:08}")).into(),
        ));
        quads.push(quad(&item, "status", node("status:open").into()));
        quads.push(quad(&item, "priority", node("priority:p2").into()));
        quads.push(quad(
            &item,
            "key",
            Literal::new_simple_literal(format!("key-{i}")).into(),
        ));
        quads.push(quad(
            &item,
            "body",
            Literal::new_simple_literal(format!("a body long enough for the string table {i}"))
                .into(),
        ));
        quads.push(quad(
            &item,
            "created",
            Literal::new_simple_literal(format!("2026-10-10T00:00:{:02}Z", i % 60)).into(),
        ));
    }
    let counter = node("counter");
    quads.push(quad(&counter, "type", node("Counter").into()));
    quads.push(quad(&counter, "last", Literal::from(items as i64).into()));
    handle.extend(quads).unwrap();
}

/// `ikigai-ledger`'s keyed append (`src/endpoints.rs`, `append`), with this file's names.
fn append(n: usize) -> String {
    let g = |body: &str| format!("GRAPH <{GRAPH}> {{ {body} }}");
    let item = format!("{NS}item:new:{n:020}");
    let key = format!("\"append-key-{n}\"");
    let triples = format!(
        "<{item}> <{NS}type> <{NS}Item> ; <{NS}number> ?new ; \
         <{NS}title> \"an appended item {n}\" ; <{NS}status> <{NS}status:open> ; \
         <{NS}priority> <{NS}priority:p2> ; <{NS}key> {key} ; \
         <{NS}body> \"a body long enough for the string table, appended {n}\" ."
    );
    format!(
        "DELETE {{ {delete} }}\nINSERT {{ {insert} }}\nWHERE {{\n  \
         {{ {{ SELECT ?last WHERE {{ {last_q} }} ORDER BY DESC(?last) LIMIT 1 }}\n    \
         UNION\n    {{ BIND(0 AS ?last) FILTER NOT EXISTS {{ {any_q} }} }} }}\n  \
         OPTIONAL {{ {old_q} }}\n  BIND(?last + 1 AS ?new)\n  FILTER NOT EXISTS {{ {taken} }}\n}}",
        delete = g(&format!("<{NS}counter> <{NS}last> ?old")),
        insert = g(&format!(
            "<{NS}counter> <{NS}type> <{NS}Counter> ; <{NS}last> ?new .\n{triples}"
        )),
        last_q = g(&format!("<{NS}counter> <{NS}last> ?last")),
        any_q = g(&format!("<{NS}counter> <{NS}last> ?any")),
        old_q = g(&format!("<{NS}counter> <{NS}last> ?old")),
        taken = g(&format!("?taken <{NS}key> {key}")),
    )
}

/// The median of `appends` appends to a graph preloaded with `items` items, and checks that
/// each one did what an append does: one item more, the counter moved by one.
fn median_append(
    open: &dyn Fn() -> (DurableStore, Arc<Store>),
    items: usize,
    appends: usize,
) -> Duration {
    let (store, handle) = open();
    preload(&handle, items);
    let kernel = kernel(store);
    let mut took: Vec<Duration> = (0..appends)
        .map(|n| {
            let started = Instant::now();
            let answer = issue(
                &kernel,
                Verb::Sink,
                "urn:iki:store:graph-update",
                &[("content", &append(n)), ("graph", GRAPH)],
            );
            let elapsed = started.elapsed();
            // The item's seven quads and the counter's new value in, its old value out.
            assert_eq!(
                answer,
                format!("updated <{GRAPH}>: +8 -1 quads\n"),
                "{answer}"
            );
            elapsed
        })
        .collect();
    let last = handle
        .quads_for_pattern(
            Some(node("counter").as_ref().into()),
            Some(node("last").as_ref()),
            None,
            None,
        )
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(last.object, Literal::from((items + appends) as i64).into());
    took.sort();
    took[took.len() / 2]
}

#[cfg(feature = "persistent")]
fn scratch(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-append-cost-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// ★ THE REGRESSION (ledger #1132): an append to a 3,000-item graph costs about what it costs
/// at 100 items. Through the whole-graph copy it cost 9x to 26x as much.
#[test]
fn an_append_costs_the_same_at_three_thousand_items_as_at_a_hundred() {
    let open = || DurableStore::in_memory_shared().unwrap();
    let small = median_append(&open, 100, 9);
    let large = median_append(&open, 3_000, 9);
    let ratio = large.as_secs_f64() / small.as_secs_f64();
    assert!(
        ratio <= MAX_RATIO,
        "an append at 3,000 items took {large:?}, {ratio:.1}x the {small:?} it takes at 100 \
         (the bound is {MAX_RATIO}x). This is the ledger #1132 shape: `src/confine.rs` is \
         copying more of the graph than the update's footprint"
    );
}

/// The measurement behind ledger #1132. `ITEMS` (default `100,750,3000`), `APPENDS` (default
/// 15) and `BACKING` (`disk`, the default with the `persistent` feature, or `memory`).
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn measure_an_append_as_the_graph_grows() {
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or(default.to_string());
    let appends: usize = var("APPENDS", "15").parse().unwrap();
    let backing = var(
        "BACKING",
        if cfg!(feature = "persistent") {
            "disk"
        } else {
            "memory"
        },
    );
    for items in var("ITEMS", "100,750,3000").split(',') {
        let items: usize = items.trim().parse().unwrap();
        let median = match backing.as_str() {
            #[cfg(feature = "persistent")]
            "disk" => {
                let path = scratch(&format!("{items}"));
                let open = || DurableStore::open_shared(&path).unwrap();
                let median = median_append(&open, items, appends);
                let _ = std::fs::remove_dir_all(&path);
                median
            }
            "memory" => median_append(
                &|| DurableStore::in_memory_shared().unwrap(),
                items,
                appends,
            ),
            other => panic!("BACKING={other}: `disk` needs --features persistent"),
        };
        println!(
            "{} build, {backing}, {items} items: median append {median:?} over {appends}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
        );
    }
}
