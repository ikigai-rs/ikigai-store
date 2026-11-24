//! The measurement of ikigai-ledger `docs/design/text-index.md` section 6, reproduced over this
//! crate's own index: a Zipf corpus of items and comments, a word scan through the store, then
//! the same words through `urn:iki:store:graph-text`. A measurement, not a CI gate:
//! `cargo bench --features text-index --bench text_index` (ITEMS=10000 for the full corpus).
use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, space, DurableStore, TextEnrollment};
use std::sync::Arc;
use std::time::Instant;

const G: &str = "urn:bench:ledger:main";

fn main() {
    let items: usize = std::env::var("ITEMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let words: Vec<String> = ["the", "of", "and", "staff", "healthiness"]
        .iter()
        .map(|w| w.to_string())
        .chain((0..30_000).map(|i| format!("w{i}")))
        .collect();
    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // Zipf-ish: rank = floor(n^u) with u uniform, s close to 1.
    let mut pick = |n: usize| {
        let u = (next() % 1_000_000) as f64 / 1_000_000.0;
        ((n as f64).powf(u) as usize).saturating_sub(1).min(n - 1)
    };
    let mut nq = String::new();
    for i in 0..items * 6 {
        let len = if i < items {
            4 + pick(8)
        } else {
            20 + pick(230)
        };
        let text: Vec<&str> = (0..len)
            .map(|_| words[pick(words.len())].as_str())
            .collect();
        let p = if i < items {
            "urn:bench:title"
        } else {
            "urn:bench:body"
        };
        nq.push_str(&format!(
            "<urn:bench:s{i}> <{p}> \"{}\" <{G}> .\n",
            text.join(" ")
        ));
    }
    let enrollment = TextEnrollment::new()
        .graph(G)
        .predicate_weighted("urn:bench:title", 2.0)
        .predicate("urn:bench:body");
    let kernel = Kernel::new(Arc::new(space(
        DurableStore::in_memory()
            .unwrap()
            .with_text_index(enrollment)
            .unwrap(),
    )));
    let issue = |verb, iri: &str, args: &[(&str, &str)]| {
        let mut r = Request::new(verb, Iri::parse(iri).unwrap());
        for (n, v) in args {
            r = r.with_arg(*n, ArgRef::Inline(v.as_bytes().to_vec()));
        }
        block_on(kernel.issue(r, &Capability::root())).map(|r| r.bytes.len())
    };
    issue(
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", &nq), ("format", "application/n-quads")],
    )
    .unwrap();
    let encoded = ikigai_store::graph_thread(G);
    let rebuild = format!(
        "urn:iki:store:text-index:graph:{}:rebuild",
        encoded.rsplit_once("graph-written:").unwrap().1
    );
    let t = Instant::now();
    issue(Verb::Sink, &rebuild, &[]).unwrap();
    println!(
        "corpus: {} literals, {:.1} MiB; rebuild {:?}",
        items * 6,
        nq.len() as f64 / 1048576.0,
        t.elapsed()
    );
    let _ = cap_read_graph(G);
    for word in ["the", "staff", "healthiness"] {
        let q = format!(
            "SELECT (COUNT(*) AS ?n) {{ ?s ?p ?o FILTER(CONTAINS(LCASE(STR(?o)), \"{word}\")) }}"
        );
        let t = Instant::now();
        issue(
            Verb::Source,
            "urn:iki:store:graph-select",
            &[("graph", G), ("query", &q), ("budget", "600")],
        )
        .unwrap();
        let scan = t.elapsed();
        let t = Instant::now();
        issue(
            Verb::Source,
            "urn:iki:store:graph-text",
            &[
                ("graph", G),
                ("words", word),
                ("limit", "50"),
                ("mode", "all"),
            ],
        )
        .unwrap();
        println!("{word}: scan {scan:?}, index top 50 {:?}", t.elapsed());
    }
}
