//! As-of reads of a named graph (ledger #3731): `as-of=T` on the scoped query forms answers
//! the graph as its history folds at T, under the same grants as a live read.
//!
//! The three the item asks for: an update after T is invisible as of T; a purge removes
//! history as `docs/design/as-of-reads.md` says; a past read is cacheable and a live one is
//! not. And the refusals that keep a past answer from being a wrong one.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Expiry, Iri, Kernel, Request, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, graph_thread, history_thread, space, DurableStore,
    SharerWrites, GRAPH_UPDATE_THREAD, LOAD_THREAD, UPDATE_THREAD,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const G: &str = "urn:example:g";
const OTHER: &str = "urn:example:other";
const NAMES: &str = "SELECT ?o WHERE { ?s ?p ?o } ORDER BY ?o";

fn kernel_over(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn issue(
    kernel: &Kernel,
    cap: &Capability,
    verb: Verb,
    iri: &str,
    args: &[(&str, &str)],
) -> ikigai_core::Result<ikigai_core::Representation> {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(iri).unwrap()),
        |request, (name, value)| request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec())),
    );
    block_on(kernel.issue(request, cap))
}

fn write(k: &Kernel, value: &str) {
    let update = format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"{value}\" }} }}");
    issue(
        k,
        &Capability::root(),
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[("graph", G), ("content", &update)],
    )
    .unwrap();
}

/// A millisecond strictly between the writes around it.
fn instant() -> u64 {
    std::thread::sleep(Duration::from_millis(5));
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::thread::sleep(Duration::from_millis(5));
    t
}

fn select(k: &Kernel, cap: &Capability, as_of: Option<&str>) -> ikigai_core::Result<String> {
    let mut args = vec![("graph", G), ("query", NAMES), ("as", "text/csv")];
    if let Some(t) = as_of {
        args.push(("as-of", t));
    }
    issue(k, cap, Verb::Source, "urn:iki:store:graph-select", &args)
        .map(|rep| String::from_utf8(rep.bytes).unwrap())
}

fn values(csv: &str) -> Vec<&str> {
    csv.lines().skip(1).map(str::trim).collect()
}

fn invalid_as_of(err: &Error) -> &str {
    match err {
        Error::InvalidArgument { name, detail } if name == "as-of" => detail,
        other => panic!("expected an `as-of` refusal, got {other:?}"),
    }
}

#[test]
fn an_update_after_t_is_invisible_as_of_t() {
    let k = kernel_over(DurableStore::in_memory().unwrap());
    let root = Capability::root();
    write(&k, "a");
    let t1 = instant().to_string();
    write(&k, "b");
    let t2 = instant().to_string();
    let remove = format!("DELETE DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"a\" }} }}");
    issue(
        &k,
        &root,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[("graph", G), ("content", &remove)],
    )
    .unwrap();

    assert_eq!(values(&select(&k, &root, Some(&t1)).unwrap()), ["a"]);
    assert_eq!(values(&select(&k, &root, Some(&t2)).unwrap()), ["a", "b"]);
    assert_eq!(values(&select(&k, &root, None).unwrap()), ["b"]);

    // CONSTRUCT too.
    let rep = issue(
        &k,
        &root,
        Verb::Source,
        "urn:iki:store:graph-construct",
        &[
            ("graph", G),
            ("query", "CONSTRUCT WHERE { ?s ?p ?o }"),
            ("as", "application/n-triples"),
            ("as-of", &t1),
        ],
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(rep.bytes).unwrap().trim(),
        "<urn:s> <urn:p> \"a\" ."
    );
}

#[test]
fn a_past_read_is_cacheable_and_a_live_one_is_not() {
    let k = kernel_over(DurableStore::in_memory().unwrap());
    let root = Capability::root();
    write(&k, "a");
    let t = instant().to_string();
    let mut args = vec![("graph", G), ("query", NAMES), ("as-of", t.as_str())];
    let past = issue(&k, &root, Verb::Source, "urn:iki:store:graph-select", &args).unwrap();
    assert!(matches!(past.expiry, Expiry::Never), "{:?}", past.expiry);
    // No write cuts it: it hangs on no write thread, only on its history's.
    let threads: Vec<&str> = past.threads().iter().map(|t| t.as_str()).collect();
    for write_thread in [UPDATE_THREAD, LOAD_THREAD, GRAPH_UPDATE_THREAD] {
        assert!(!threads.contains(&write_thread), "{threads:?}");
    }
    assert!(!threads.contains(&graph_thread(G).as_str()), "{threads:?}");
    assert!(threads.contains(&history_thread(G).as_str()), "{threads:?}");

    // On a store a sharer may write, a live read is never cached, and a past one of a graph
    // the sharer may write is refused rather than answered from a history it cannot see.
    let (shared, _handle) = DurableStore::in_memory_shared().unwrap();
    let k = kernel_over(shared);
    args.pop();
    let live = issue(&k, &root, Verb::Source, "urn:iki:store:graph-select", &args).unwrap();
    assert!(matches!(live.expiry, Expiry::Always), "{:?}", live.expiry);
    let err = select(&k, &root, Some(&t)).unwrap_err();
    assert!(invalid_as_of(&err).contains("writer outside"), "{err:?}");
}

#[test]
fn a_live_read_on_an_owned_store_still_hangs_on_its_threads() {
    let k = kernel_over(DurableStore::in_memory().unwrap());
    let root = Capability::root();
    write(&k, "a");
    let live = issue(
        &k,
        &root,
        Verb::Source,
        "urn:iki:store:graph-select",
        &[("graph", G), ("query", NAMES)],
    )
    .unwrap();
    // Cacheable only until the next write: it names the threads a write cuts.
    let threads: Vec<&str> = live.threads().iter().map(|t| t.as_str()).collect();
    assert!(threads.contains(&graph_thread(G).as_str()), "{threads:?}");
    assert!(threads.contains(&UPDATE_THREAD), "{threads:?}");
}

#[test]
fn a_purge_removes_history_and_a_compaction_keeps_what_it_folds() {
    let store = DurableStore::in_memory().unwrap();
    let kept = store.clone();
    let k = kernel_over(store);
    let root = Capability::root();
    write(&k, "a");
    let t1 = instant();
    write(&k, "b");
    let t2 = instant();
    write(&k, "c");
    let t3 = instant();
    assert_eq!(kept.history_entries(G), 3);

    // Compaction at t2: as of t2 and later unchanged, earlier refused, one entry fewer.
    kept.compact_history(G, t2);
    k.cut(history_thread(G));
    assert_eq!(kept.history_entries(G), 2);
    assert_eq!(
        values(&select(&k, &root, Some(&t2.to_string())).unwrap()),
        ["a", "b"]
    );
    assert_eq!(
        values(&select(&k, &root, Some(&t3.to_string())).unwrap()),
        ["a", "b", "c"]
    );
    let err = select(&k, &root, Some(&t1.to_string())).unwrap_err();
    assert!(invalid_as_of(&err).contains("no history"), "{err:?}");

    // A purge: every past read refused, the live graph untouched.
    assert!(kept.purge_history(G));
    k.cut(history_thread(G));
    assert_eq!(kept.history_entries(G), 0);
    let err = select(&k, &root, Some(&t3.to_string())).unwrap_err();
    assert!(invalid_as_of(&err).contains("no history"), "{err:?}");
    assert_eq!(values(&select(&k, &root, None).unwrap()), ["a", "b", "c"]);

    // The next scoped write starts a new history from the graph as it then is.
    write(&k, "d");
    let t4 = instant();
    assert_eq!(
        values(&select(&k, &root, Some(&t4.to_string())).unwrap()),
        ["a", "b", "c", "d"]
    );
    let err = select(&k, &root, Some(&t3.to_string())).unwrap_err();
    assert!(invalid_as_of(&err).contains("no history"), "{err:?}");
}

#[test]
fn a_broad_write_breaks_history_until_the_next_scoped_write() {
    let k = kernel_over(DurableStore::in_memory().unwrap());
    let root = Capability::root();
    write(&k, "a");
    let before = instant();
    let broad = format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:s> <urn:p> \"x\" }} }}");
    issue(
        &k,
        &root,
        Verb::Sink,
        "urn:iki:store:update",
        &[("content", &broad)],
    )
    .unwrap();
    let gap = instant();
    // Before the broad write: still answered, and right.
    assert_eq!(
        values(&select(&k, &root, Some(&before.to_string())).unwrap()),
        ["a"]
    );
    // After it: refused, not answered without the quad the log never saw.
    let err = select(&k, &root, Some(&gap.to_string())).unwrap_err();
    assert!(invalid_as_of(&err).contains("no history"), "{err:?}");
    // A scoped write takes a fresh base, which includes it.
    write(&k, "b");
    let after = instant();
    assert_eq!(
        values(&select(&k, &root, Some(&after.to_string())).unwrap()),
        ["a", "b", "x"]
    );
}

#[test]
fn a_past_read_needs_the_same_grant_as_a_live_one() {
    let k = kernel_over(DurableStore::in_memory().unwrap());
    write(&k, "a");
    let t = instant().to_string();
    let other = Capability::scoped([cap_read_graph(OTHER)]);
    assert!(matches!(
        select(&k, &other, Some(&t)).unwrap_err(),
        Error::Denied(_)
    ));
    let writer = Capability::scoped([cap_write_graph(G)]);
    assert!(select(&k, &writer, Some(&t)).is_err());
    let reader = Capability::scoped([cap_read_graph(G)]);
    assert_eq!(values(&select(&k, &reader, Some(&t)).unwrap()), ["a"]);
}

#[test]
fn a_time_not_in_the_past_and_a_broad_form_are_refused() {
    let k = kernel_over(DurableStore::in_memory().unwrap());
    let root = Capability::root();
    write(&k, "a");
    let future = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000)
        .to_string();
    let err = select(&k, &root, Some(&future)).unwrap_err();
    assert!(invalid_as_of(&err).contains("not in the past"), "{err:?}");
    let err = select(&k, &root, Some("yesterday")).unwrap_err();
    assert!(invalid_as_of(&err).contains("not a time"), "{err:?}");
    let err = issue(
        &k,
        &root,
        Verb::Source,
        "urn:iki:store:select",
        &[("query", NAMES), ("as-of", "1")],
    )
    .unwrap_err();
    assert!(invalid_as_of(&err).contains("graph-select"), "{err:?}");
    // Before the graph's first recorded write.
    let err = select(&k, &root, Some("1")).unwrap_err();
    assert!(invalid_as_of(&err).contains("no history"), "{err:?}");
}

#[test]
fn a_graph_a_declared_sharer_does_not_write_keeps_its_history() {
    let (store, _handle) =
        DurableStore::in_memory_shared_declaring(SharerWrites::only_the_default_graph()).unwrap();
    let k = kernel_over(store);
    let root = Capability::root();
    write(&k, "a");
    let t = instant().to_string();
    write(&k, "b");
    assert_eq!(values(&select(&k, &root, Some(&t)).unwrap()), ["a"]);
}
