//! `open_shared_declaring`: what a host's promise buys, what it costs when it is wrong,
//! and the tripwire that keeps it from being wrong silently.
//!
//! # What is under test
//!
//! `open_shared` sets one bit for the whole store and every read goes bare. That is
//! right for a crate that cannot see who else holds the handle — and it is expensive in
//! a way nothing catches, because expiry PROPAGATES: a module whose reads are sub-
//! requests to `urn:iki:store:graph-select` loses its own caching as a side effect of
//! its host adding a second face over the same dataset. `ikigai-gonk` measured ~1000× on
//! a 247-item ledger and recovered it from outside this crate.
//!
//! `SharerWrites` is that recovery, made here where the argument's terms live: a scoped
//! read's universe is ONE named graph by construction (`src/scope.rs`), so a sharer that
//! cannot write that graph cannot change that read's answer.
//!
//! # ★ Every test is a pair, for the reason `tests/read_scope.rs` states
//!
//! A cache hit and a cache miss return the same bytes when nothing has changed, so
//! "it is cached" is not directly observable through the kernel door. What IS observable
//! is **what a write does next**, so each claim here is pinned from both sides:
//!
//! - a write through the KERNEL cuts the threads, so a covered read recomputes;
//! - a write through the RAW HANDLE cuts nothing, so a covered read does NOT — which is
//!   exactly the staleness a false promise buys, and is asserted as such.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, space, DurableStore, SharerWrites, Store, CAP_READ, CAP_WRITE,
};
use oxigraph::io::RdfFormat;
use std::sync::Arc;

/// The tenant graph a host's module owns — reserved from the sharer in most tests here.
const G: &str = "urn:example:acme";
/// A second graph, so "reserved" and "permitted" can be told apart in one store.
const OTHER: &str = "urn:example:zenith";

fn kernel(store: DurableStore) -> Kernel {
    Kernel::new(Arc::new(space(store)))
}

fn issue(
    kernel: &Kernel,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, cap)).map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

/// A scoped SELECT over `graph`, as CSV so a row is a line.
fn scoped_rows(kernel: &Kernel, graph: &str) -> Vec<String> {
    issue(
        kernel,
        Verb::Source,
        "urn:iki:store:graph-select",
        &[
            ("graph", graph),
            ("query", "SELECT ?s WHERE { ?s ?p ?o }"),
            ("as", "text/csv"),
        ],
        &Capability::scoped([cap_read_graph(graph)]),
    )
    .expect("a scoped read")
    .lines()
    .skip(1)
    .map(str::to_string)
    .collect()
}

/// The broad SELECT, over the whole dataset.
///
/// ⚠ The UNION is not decoration: a bare `{ ?s ?p ?o }` reads the DEFAULT graph only —
/// SPARQL's rule, and `src/scope.rs` documents the same fact from the other side — so a
/// broad query that wanted every quad has to ask for the named graphs too.
fn broad_rows(kernel: &Kernel) -> Vec<String> {
    issue(
        kernel,
        Verb::Source,
        "urn:iki:store:select",
        &[
            (
                "query",
                "SELECT ?s WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }",
            ),
            ("as", "text/csv"),
        ],
        &Capability::scoped([CAP_READ.to_string()]),
    )
    .expect("the broad read")
    .lines()
    .skip(1)
    .map(str::to_string)
    .collect()
}

fn info(kernel: &Kernel) -> String {
    issue(
        kernel,
        Verb::Source,
        "urn:iki:store:info",
        &[],
        &Capability::scoped([CAP_READ.to_string()]),
    )
    .expect("info")
}

/// A write the kernel never sees — the sharer, in one line.
fn sharer_writes(handle: &Store, nquads: &str) {
    handle
        .load_from_slice(RdfFormat::NQuads, nquads.as_bytes())
        .expect("the sharer's write");
}

/// One quad in each tenant graph, written through the handle so the store starts in the
/// state a host's would: populated by whoever populated it, with nothing cached yet.
fn seeded(writes: SharerWrites) -> (DurableStore, Arc<Store>) {
    let (store, handle) = DurableStore::in_memory_shared_declaring(writes).unwrap();
    sharer_writes(
        &handle,
        &format!(
            "<urn:example:acme:1> <urn:p> \"acme\" <{G}> .\n\
             <urn:example:zenith:1> <urn:p> \"zenith\" <{OTHER}> .\n\
             <urn:example:host:1> <urn:p> \"host\" .\n"
        ),
    );
    (store, handle)
}

// ------------------------------------------------------------------ what it buys

/// ★★ The arc, executable. On a store whose sharer writes only the default graph, a
/// scoped read is cached — and a write through this crate's own scoped door still cuts
/// it, because the three write threads are untouched.
///
/// The pair: the kernel write below is seen, and
/// `a_declared_promise_that_is_broken_is_silent_staleness` shows the raw-handle write to
/// the same graph is NOT. Together they say the read really was cached and really was
/// invalidated by everything that can legitimately change it.
#[test]
fn a_scoped_read_of_a_reserved_graph_is_cached_and_the_kernel_still_cuts_it() {
    let (store, _handle) = seeded(SharerWrites::only_the_default_graph());
    let k = kernel(store);

    assert_eq!(scoped_rows(&k, G), ["urn:example:acme:1"]);
    assert_eq!(scoped_rows(&k, G), ["urn:example:acme:1"], "byte-identical");

    issue(
        &k,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", G),
            (
                "content",
                &format!(
                    "INSERT DATA {{ GRAPH <{G}> {{ <urn:example:acme:2> <urn:p> \"new\" }} }}"
                ),
            ),
        ],
        &Capability::scoped([cap_write_graph(G)]),
    )
    .expect("the scoped write");

    let rows = scoped_rows(&k, G);
    assert!(
        rows.contains(&"urn:example:acme:2".to_string()),
        "a scoped write must cut GRAPH_UPDATE_THREAD and invalidate the cached read: {rows:?}"
    );
}

/// ⚠⚠ **The cost of a false promise, as an assertion.** The declaration says the sharer
/// writes only the default graph; here it writes `G` anyway. The cached read keeps
/// serving the pre-write answer — no error, no log line, no expiry to wait out. This is
/// what `SharerWrites`'s docs mean by "stale with no bound and no signal", and it is
/// pinned so the cost stays a fact rather than a warning.
///
/// A host that cannot rule this out uses `open_shared`, whose blanket is merely slow.
#[test]
fn a_declared_promise_that_is_broken_is_silent_staleness() {
    let (store, handle) = seeded(SharerWrites::only_the_default_graph());
    let k = kernel(store);

    assert_eq!(scoped_rows(&k, G), ["urn:example:acme:1"]);
    sharer_writes(
        &handle,
        &format!("<urn:example:acme:99> <urn:p> \"broken\" <{G}> .\n"),
    );

    assert_eq!(
        scoped_rows(&k, G),
        ["urn:example:acme:1"],
        "the promise was that this could not happen; when it does, the staleness is \
         unbounded and silent — which is why `reserved_graphs_fingerprint` exists"
    );
}

/// The sharer's own graph is bare, in the same store where another graph is cached. This
/// is the second case a host must be able to spell: `ikigai-browse` after `Mount::graph`.
#[test]
fn a_graph_the_sharer_may_write_is_never_cached() {
    let (store, handle) = seeded(
        SharerWrites::only_the_default_graph().and_named_graph(OTHER), // the sharer's own
    );
    let k = kernel(store);

    assert_eq!(scoped_rows(&k, OTHER), ["urn:example:zenith:1"]);
    sharer_writes(
        &handle,
        &format!("<urn:example:zenith:2> <urn:p> \"live\" <{OTHER}> .\n"),
    );
    let rows = scoped_rows(&k, OTHER);
    assert!(
        rows.contains(&"urn:example:zenith:2".to_string()),
        "a declared graph must stay bare — the sharer writes it and nothing cuts a thread: {rows:?}"
    );

    // …and the graph NOT declared is cached in the very same store, which is the whole
    // point of a per-graph decision.
    assert_eq!(scoped_rows(&k, G), ["urn:example:acme:1"]);
    sharer_writes(
        &handle,
        &format!("<urn:example:acme:99> <urn:p> \"stale\" <{G}> .\n"),
    );
    assert_eq!(
        scoped_rows(&k, G),
        ["urn:example:acme:1"],
        "the reserved graph is cached (and this write broke the promise about it)"
    );
}

/// ★ The broad faces see the whole dataset, default graph included, so they stay bare
/// whatever was declared. Nothing `SharerWrites` can say makes them safe, because the
/// sharer may always write the default graph — that is what sharing the handle is for.
#[test]
fn the_broad_faces_and_info_stay_bare_whatever_was_declared() {
    let (store, handle) = seeded(SharerWrites::only_the_default_graph());
    let k = kernel(store);

    assert_eq!(broad_rows(&k).len(), 3);
    let first = info(&k);
    sharer_writes(&handle, "<urn:example:host:2> <urn:p> \"more\" .\n");

    assert_eq!(
        broad_rows(&k).len(),
        4,
        "a broad read must not be cached on a shared store: it can see the default graph"
    );
    assert_ne!(
        first,
        info(&k),
        "`urn:iki:store:info` counts every quad, so it can see the sharer's write too"
    );
}

/// The declaration is legible from outside the process, and only when there is one.
#[test]
fn info_names_the_declaration_and_says_nothing_extra_without_one() {
    let (declared, _h) = seeded(SharerWrites::only_the_default_graph().and_named_graph(OTHER));
    let text = info(&kernel(declared));
    assert!(text.contains("covered: false"), "{text}");
    assert!(
        text.contains(&format!("sharer writes: the default graph and <{OTHER}>")),
        "{text}"
    );

    let (undeclared, _h) = DurableStore::in_memory_shared().unwrap();
    let text = info(&kernel(undeclared));
    assert!(text.contains("covered: false"), "{text}");
    assert!(
        !text.contains("sharer writes"),
        "an undeclared shared store's bytes are unchanged from 0.2.3: {text}"
    );
}

/// ⚠ **`open_shared` must not have changed.** `ikigai-gonk` pins 0.2.3 and depends on
/// the blanket; the finer control is additive or it is a flag day. Here the same
/// raw-handle write that goes stale under a declaration is seen immediately without one.
#[test]
fn an_undeclared_shared_store_still_caches_nothing_at_all() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    sharer_writes(
        &handle,
        &format!("<urn:example:acme:1> <urn:p> \"acme\" <{G}> .\n"),
    );
    let k = kernel(store);

    assert_eq!(scoped_rows(&k, G), ["urn:example:acme:1"]);
    sharer_writes(
        &handle,
        &format!("<urn:example:acme:2> <urn:p> \"second\" <{G}> .\n"),
    );
    let rows = scoped_rows(&k, G);
    assert_eq!(
        rows.len(),
        2,
        "without a declaration every read is Expiry::Always, exactly as in 0.2.3: {rows:?}"
    );
}

/// An owned store is untouched by any of this: it has no sharer, so it declares nothing
/// and caches everything, broad faces included.
#[test]
fn an_owned_store_is_unaffected() {
    let store = DurableStore::in_memory().unwrap();
    assert!(store.is_sole_writer());
    assert!(store.sharer_writes().is_none());
    assert!(store.read_is_covered(None) && store.read_is_covered(Some(G)));

    let k = kernel(store);
    issue(
        &k,
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", "<urn:example:host:1> <urn:p> \"host\" .")],
        &Capability::scoped([CAP_WRITE.to_string()]),
    )
    .expect("a load");
    assert_eq!(broad_rows(&k), ["urn:example:host:1"]);
    assert!(!info(&k).contains("sharer writes"), "nothing was shared");
}

// --------------------------------------------------------------------- the tripwire

/// ★ The promise made checkable. This is the assertion a host puts in its own tests, and
/// the reason this crate offers it rather than leaving each host to invent one:
/// `ikigai-gonk` wrote `a_browse_write_touches_no_named_graph` over the SET of named
/// graph names, which catches a sharer that creates a graph and misses one that writes
/// into a graph that already exists. This catches both.
#[test]
fn the_tripwire_is_silent_on_a_kept_promise_and_names_the_graph_on_a_broken_one() {
    let (store, handle) = seeded(SharerWrites::only_the_default_graph());

    let before = store.reserved_graphs_fingerprint().unwrap();
    sharer_writes(&handle, "<urn:example:host:2> <urn:p> \"kept\" .\n");
    assert!(
        store
            .reserved_graphs_fingerprint()
            .unwrap()
            .changed_since(&before)
            .is_empty(),
        "a write to the default graph is exactly what was promised"
    );

    // The miss gonk's own tripwire has: writing into a graph that ALREADY EXISTS leaves
    // the set of named graph names unchanged. The fingerprint is over quads, so it sees
    // it.
    sharer_writes(
        &handle,
        &format!("<urn:example:acme:2> <urn:p> \"broken\" <{G}> .\n"),
    );
    assert_eq!(
        store
            .reserved_graphs_fingerprint()
            .unwrap()
            .changed_since(&before),
        [format!("<{G}>")],
        "the tripwire must name the graph a host has to go look at"
    );
}

/// A graph that APPEARS is a change too — and so is one that is emptied away, which is
/// how a `DROP GRAPH` through the handle shows up.
#[test]
fn the_tripwire_catches_a_graph_that_appears_and_one_that_vanishes() {
    let (store, handle) = seeded(SharerWrites::only_the_default_graph());
    let before = store.reserved_graphs_fingerprint().unwrap();
    assert_eq!(before.len(), 2, "both tenant graphs are reserved");
    assert!(!before.is_empty());

    sharer_writes(
        &handle,
        "<urn:example:new:1> <urn:p> \"x\" <urn:example:new> .\n",
    );
    assert_eq!(
        store
            .reserved_graphs_fingerprint()
            .unwrap()
            .changed_since(&before),
        ["<urn:example:new>"]
    );

    handle
        .remove_named_graph(oxigraph::model::NamedNodeRef::new(G).unwrap())
        .expect("the sharer drops a graph it promised not to touch");
    let changed = store
        .reserved_graphs_fingerprint()
        .unwrap()
        .changed_since(&before);
    assert!(
        changed.contains(&format!("<{G}>")),
        "a graph that vanished must be reported: {changed:?}"
    );
}

/// ★ A fingerprint over a store that promised nothing would be a gate that passes
/// vacuously — the failure family constitution 9c is about. It refuses instead, in both
/// directions.
#[test]
fn the_tripwire_refuses_a_store_that_declared_nothing() {
    for (store, why) in [
        (DurableStore::in_memory().unwrap(), "never left"),
        (DurableStore::in_memory_shared().unwrap().0, "undeclared"),
    ] {
        let err = store
            .reserved_graphs_fingerprint()
            .expect_err("no declaration, no tripwire");
        assert!(
            matches!(err, Error::Endpoint(_)),
            "{why}: expected a legible refusal, got {err:?}"
        );
        assert!(
            err.to_string().contains("open_shared_declaring"),
            "{why}: the refusal must say how to get a fingerprint: {err}"
        );
    }
}

/// A graph the sharer may write is NOT fingerprinted: it is expected to change, and
/// including it would make the tripwire cry on a promise that was kept.
#[test]
fn a_permitted_graph_is_not_in_the_fingerprint() {
    let (store, handle) = seeded(SharerWrites::only_the_default_graph().and_named_graph(OTHER));
    let before = store.reserved_graphs_fingerprint().unwrap();
    assert_eq!(
        before.len(),
        1,
        "only the reserved tenant graph: {before:?}"
    );

    sharer_writes(
        &handle,
        &format!("<urn:example:zenith:2> <urn:p> \"expected\" <{OTHER}> .\n"),
    );
    assert!(store
        .reserved_graphs_fingerprint()
        .unwrap()
        .changed_since(&before)
        .is_empty());
}

// -------------------------------------------------------------------- measurement

/// ⚠ **The measurement, kept runnable and kept OUT of CI** — like `m1` in
/// `tests/handle_model.rs`, and for the same reason: it builds a corpus and times reads,
/// which is noise in a CI runner and minutes of nothing useful.
///
/// ```text
/// cargo test --test shared_coverage -- --ignored --nocapture
/// ```
///
/// The corpus is shaped like the one that produced the problem: a ledger-sized tenant
/// graph (250 items × 8 quads) that a module reads through the scoped face, beside a
/// browse-sized body of quads in the default graph that the sharer owns. The three rows
/// are the three modes, on the same corpus, through the same kernel door.
///
/// Measured 2026-09-16 (Apple M5 Max, debug profile, mean of 20 reads, three runs within
/// ~4% of each other):
///
/// | mode | scoped SELECT, 250 items |
/// | --- | --- |
/// | `open` (covered) | 32.5 µs |
/// | `open_shared` (0.2.3) | **1.05 ms** |
/// | `open_shared_declaring(only_the_default_graph())` | **31.1 µs** |
///
/// ~34× on this query, and the declared mode lands on the owned baseline — as it must,
/// since past `with_freshness` the two are the same code. ⚠ The RATIO is a property of
/// the query, not of this crate: `ikigai-gonk` measured ~1000× because a ledger's `items`
/// and `next` do far more work per read than one join. What is constant is that the
/// uncached row is the full query every time and the cached row is not.
#[test]
#[ignore = "benchmark: builds a corpus and times reads; run explicitly"]
fn measure_the_cost_of_sharing() {
    use std::time::Instant;

    const ITEMS: usize = 250;
    const BROWSE_QUADS: usize = 2_000;
    const ROUNDS: u32 = 20;

    // One ledger item is 8 quads in the tenant graph; one browse annotation is a triple
    // in the default graph. Both go in through the raw handle, which is how a host that
    // shares its store populates it anyway.
    let corpus = {
        let mut nq = String::with_capacity(ITEMS * 8 * 90 + BROWSE_QUADS * 70);
        for i in 0..ITEMS {
            for f in 0..8 {
                nq.push_str(&format!(
                    "<urn:iki:ledger:default:item:{i}> <urn:iki:ledger:p{f}> \"value {i}-{f}\" <{G}> .\n"
                ));
            }
        }
        for i in 0..BROWSE_QUADS {
            nq.push_str(&format!(
                "<urn:iki:browse:note:{i}> <urn:iki:browse:body> \"note {i}\" .\n"
            ));
        }
        nq
    };

    // The shape a ledger's `items` really resolves: every item with a couple of its
    // fields, joined, inside one tenant graph.
    let query = "SELECT ?item ?a ?b WHERE { \
                 ?item <urn:iki:ledger:p0> ?a . ?item <urn:iki:ledger:p1> ?b }";

    let time = |label: &str, store: DurableStore, handle: Option<Arc<Store>>| {
        let k = kernel(store);
        // One warm read first: the first read pays for the query either way, and what is
        // being measured is the SECOND one — which is a cache hit or is not.
        let _ = issue(
            &k,
            Verb::Source,
            "urn:iki:store:graph-select",
            &[("graph", G), ("query", query), ("as", "text/csv")],
            &Capability::scoped([cap_read_graph(G)]),
        )
        .expect("the warm read");
        let start = Instant::now();
        for _ in 0..ROUNDS {
            let rows = issue(
                &k,
                Verb::Source,
                "urn:iki:store:graph-select",
                &[("graph", G), ("query", query), ("as", "text/csv")],
                &Capability::scoped([cap_read_graph(G)]),
            )
            .expect("a scoped read");
            assert_eq!(rows.lines().count(), ITEMS + 1, "header + one row per item");
        }
        println!("{label:38} {:?}", start.elapsed() / ROUNDS);
        drop(handle);
    };

    let owned = DurableStore::in_memory().unwrap();
    {
        // The owned store has no handle to populate through, so it is seeded through its
        // own Sink — the same bytes by the covered door.
        let k = kernel(owned.clone());
        issue(
            &k,
            Verb::Sink,
            "urn:iki:store:load",
            &[("content", &corpus), ("format", "application/n-quads")],
            &Capability::scoped([CAP_WRITE.to_string()]),
        )
        .expect("seeding the owned store");
    }
    time("open (covered)", owned, None);

    let (shared, handle) = DurableStore::in_memory_shared().unwrap();
    sharer_writes(&handle, &corpus);
    time("open_shared (as shipped in 0.2.3)", shared, Some(handle));

    let (declared, handle) =
        DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph()).unwrap();
    sharer_writes(&handle, &corpus);
    time("open_shared_declaring", declared, Some(handle));
}
