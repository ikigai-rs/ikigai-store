//! Measure a LIMIT-paged SELECT of the shape a ledger listing uses (ledger #4418): a few
//! thousand items, `ORDER BY`, `LIMIT 50`, read twice through `urn:iki:store:select`.
//!
//! The first read is computed. The second is served from the kernel cache when the store
//! declares the answer cacheable, and computed again when it does not. Each round varies a
//! comment in the query text, so each round is a fresh cache key on one kernel and store.
//!
//! `cargo run --release --example paged-read -- [items] [rounds]`

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut args = std::env::args().skip(1);
    let items: usize = args.next().map_or(3000, |a| a.parse().unwrap());
    let rounds: usize = args.next().map_or(50, |a| a.parse().unwrap());

    // An owned store, loaded through its own door: a shared one is never cached at all.
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    let mut turtle = String::new();
    for i in 0..items {
        let state = if i % 3 == 0 { "closed" } else { "open" };
        let created = i as u64 * 7919 % 1_000_003;
        turtle.push_str(&format!(
            "<urn:ledger:item:{i}> a <urn:ledger:Item> ; <urn:ledger:title> \"item {i}\" ; \
             <urn:ledger:created> {created} ; <urn:ledger:state> \"{state}\" .\n"
        ));
    }
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
                .with_arg("content", ArgRef::Inline(turtle.into_bytes()))
                .with_arg("graph", ArgRef::Inline(b"urn:ledger:items".to_vec())),
            &Capability::root(),
        ),
    )
    .unwrap();

    let mut first = Vec::new();
    let mut second = Vec::new();
    let mut rows = 0;
    let mut cached = 0;
    for round in 0..rounds {
        let query = format!(
            "# round {round}
             SELECT ?item ?title ?created WHERE {{
               GRAPH <urn:ledger:items> {{
                 ?item a <urn:ledger:Item> ; <urn:ledger:title> ?title ;
                       <urn:ledger:created> ?created ; <urn:ledger:state> \"open\" .
               }}
             }} ORDER BY DESC(?created) LIMIT 50"
        );
        let request = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
            .with_arg("query", ArgRef::Inline(query.into_bytes()))
            .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()));
        for times in [&mut first, &mut second] {
            let start = Instant::now();
            let answer = block_on(kernel.issue(request.clone(), &Capability::root())).unwrap();
            times.push(start.elapsed());
            rows = String::from_utf8(answer.bytes).unwrap().lines().count() - 1;
        }
        if kernel.is_cached(&request, &Capability::root()) {
            cached += 1;
        }
    }
    println!("items {items}, rounds {rounds}, rows per page {rows}, cached after the first read: {cached}/{rounds}");
    report("first read ", &mut first);
    report("second read", &mut second);
}

fn report(label: &str, times: &mut [Duration]) {
    times.sort();
    let at = |p: f64| times[((times.len() - 1) as f64 * p).round() as usize];
    println!(
        "{label}: median {:>9.3?}  p90 {:>9.3?}  min {:>9.3?}  max {:>9.3?}",
        at(0.5),
        at(0.9),
        times[0],
        times[times.len() - 1]
    );
}
