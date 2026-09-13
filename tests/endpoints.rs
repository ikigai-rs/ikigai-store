//! What the seven resources promise, held to it through the kernel.
//!
//! Everything here runs on an in-memory `DurableStore`, which is the same code path as
//! the durable one for every property under test — capability enforcement, thread cuts,
//! `as` refusal and the coverage rule are all above the backing. The facts that are
//! genuinely about RocksDB live in `tests/handle_model.rs` behind the `persistent`
//! feature.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, CAP_READ, CAP_WRITE, LOAD_THREAD, UPDATE_THREAD};
use std::sync::Arc;

fn kernel(store: DurableStore) -> Kernel {
    Kernel::with_meta_renderer(
        Arc::new(space(store)),
        Arc::new(ikigai_vocab::TurtleRenderer),
    )
}

fn iri(s: &str) -> Iri {
    Iri::parse(s).unwrap()
}

/// A capability holding exactly the named scopes and nothing else.
fn cap(scopes: &[&str]) -> Capability {
    Capability::scoped(scopes.iter().map(|s| s.to_string()))
}

fn text(rep: ikigai_core::Representation) -> String {
    String::from_utf8(rep.bytes).unwrap()
}

fn load(kernel: &Kernel, cap: &Capability, turtle: &str) -> ikigai_core::Result<String> {
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, iri("urn:iki:store:load"))
                .with_arg("content", ArgRef::Inline(turtle.as_bytes().to_vec())),
            cap,
        ),
    )
    .map(text)
}

/// Resolve one query form. The form is the IRI here, exactly as a caller experiences it.
fn ask_form(
    kernel: &Kernel,
    cap: &Capability,
    form: &str,
    sparql: &str,
) -> ikigai_core::Result<String> {
    block_on(
        kernel.issue(
            Request::new(Verb::Source, iri(&format!("urn:iki:store:{form}")))
                .with_arg("query", ArgRef::Inline(sparql.as_bytes().to_vec())),
            cap,
        ),
    )
    .map(text)
}

fn select(kernel: &Kernel, cap: &Capability, sparql: &str) -> ikigai_core::Result<String> {
    ask_form(kernel, cap, "select", sparql)
}

fn ask(kernel: &Kernel, cap: &Capability, sparql: &str) -> ikigai_core::Result<String> {
    ask_form(kernel, cap, "ask", sparql)
}

// ------------------------------------------------------------------ the happy paths

#[test]
fn a_document_loaded_through_the_kernel_is_queryable() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ, CAP_WRITE]);
    let report = load(
        &k,
        &c,
        r#"@prefix ex: <http://ex/> . ex:a ex:name "Alice" . ex:b ex:name "Bob" ."#,
    )
    .unwrap();
    assert!(report.contains("0 -> 2 quads"), "{report}");

    let json = select(&k, &c, "SELECT ?name WHERE { ?s <http://ex/name> ?name }").unwrap();
    assert!(json.contains("Alice") && json.contains("Bob"), "{json}");
}

#[test]
fn an_update_through_the_kernel_changes_what_a_query_sees() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ, CAP_WRITE]);
    let report = text(
        block_on(k.issue(
            Request::new(Verb::Sink, iri("urn:iki:store:update")).with_arg(
                "content",
                ArgRef::Inline(
                    b"INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> }".to_vec(),
                ),
            ),
            &c,
        ))
        .unwrap(),
    );
    assert!(report.contains("0 -> 1 quads"), "{report}");
    assert!(ask(&k, &c, "ASK { ?s ?p ?o }").unwrap().contains("true"));
}

#[test]
fn info_reports_the_backing_and_whether_coverage_holds() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ]);
    let info =
        text(block_on(k.issue(Request::new(Verb::Source, iri("urn:iki:store:info")), &c)).unwrap());
    assert!(info.contains("backing: memory"), "{info}");
    assert!(info.contains("covered: true"), "{info}");
}

// -------------------------------------------------------- declared = enforced (both)

/// ★ The ablation for condition 3, in both directions. Delete `.requires(CAP_READ)` from
/// `store-query`'s `describe()` and the first half of this fails; delete
/// `.requires(CAP_WRITE)` from `store-load` and the second does.
///
/// A read is gated **because this store holds standing state the caller did not name** —
/// the difference from a query module whose dataset is assembled per call from sources
/// the caller listed.
#[test]
fn a_caller_without_the_scope_is_denied_in_both_directions() {
    let k = kernel(DurableStore::in_memory().unwrap());

    let write_only = cap(&[CAP_WRITE]);
    let err = select(&k, &write_only, "SELECT * { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(err, ikigai_core::Error::Denied(_)),
        "a read must be typed-Denied without {CAP_READ}, got {err:?}"
    );

    let read_only = cap(&[CAP_READ]);
    let err = load(
        &k,
        &read_only,
        "<http://ex/a> <http://ex/p> <http://ex/b> .",
    )
    .unwrap_err();
    assert!(
        matches!(err, ikigai_core::Error::Denied(_)),
        "a write must be typed-Denied without {CAP_WRITE}, got {err:?}"
    );

    let err = block_on(
        k.issue(
            Request::new(Verb::Sink, iri("urn:iki:store:update"))
                .with_arg("content", ArgRef::Inline(b"CLEAR ALL".to_vec())),
            &read_only,
        ),
    )
    .unwrap_err();
    assert!(
        matches!(err, ikigai_core::Error::Denied(_)),
        "CLEAR ALL must be typed-Denied without {CAP_WRITE}, got {err:?}"
    );
}

// ------------------------------------------------------------------- golden threads

/// ★★ The coverage argument, executable. A covered store's read is a cache HIT on the
/// second call and recomputes after either writing IRI is sunk — because under
/// `DurableStore::open` there is no third way to write.
#[test]
fn a_covered_read_caches_and_both_writing_iris_cut_it() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ, CAP_WRITE]);

    let first = select(&k, &c, "SELECT ?s WHERE { ?s ?p ?o }").unwrap();
    let second = select(&k, &c, "SELECT ?s WHERE { ?s ?p ?o }").unwrap();
    assert_eq!(first, second, "a cacheable read must be byte-identical");

    // A load cuts LOAD_THREAD, so the cached read recomputes and now sees data.
    load(&k, &c, "<http://ex/a> <http://ex/p> <http://ex/b> .").unwrap();
    let after_load = select(&k, &c, "SELECT ?s WHERE { ?s ?p ?o }").unwrap();
    assert!(
        after_load.contains("http://ex/a"),
        "a load must cut {LOAD_THREAD} and invalidate the cached read: {after_load}"
    );

    // An update cuts UPDATE_THREAD — the other name, and a reader must depend on both.
    block_on(k.issue(
        Request::new(Verb::Sink, iri("urn:iki:store:update")).with_arg(
            "content",
            ArgRef::Inline(b"INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }".to_vec()),
        ),
        &c,
    ))
    .unwrap();
    let after_update = select(&k, &c, "SELECT ?s WHERE { ?s ?p ?o }").unwrap();
    assert!(
        after_update.contains("http://ex/c"),
        "an update must cut {UPDATE_THREAD} and invalidate the cached read: {after_update}"
    );
}

/// ★ The other half, and the reason the two modes are two constructors: a store whose
/// handle was handed out serves reads `Expiry::Always`, forever. Here the raw handle
/// writes — a writer the kernel never saw — and the very next read still sees it,
/// because nothing was cached to go stale.
#[test]
fn a_shared_store_never_caches_so_a_raw_handle_write_is_still_visible() {
    let (store, handle) = DurableStore::in_memory_shared().unwrap();
    let k = kernel(store);
    let c = cap(&[CAP_READ]);

    assert!(ask(&k, &c, "ASK { ?s ?p ?o }").unwrap().contains("false"));
    handle
        .load_from_slice(
            oxigraph::io::RdfFormat::NTriples,
            b"<http://ex/a> <http://ex/p> <http://ex/b> .",
        )
        .unwrap();
    assert!(
        ask(&k, &c, "ASK { ?s ?p ?o }").unwrap().contains("true"),
        "a shared store must not cache: the raw-handle write cut nothing"
    );

    let info =
        text(block_on(k.issue(Request::new(Verb::Source, iri("urn:iki:store:info")), &c)).unwrap());
    assert!(info.contains("covered: false"), "{info}");
}

// ----------------------------------------------------------------- the `as` selector

/// A bound must refuse, not substitute. `as=text/turtle` on a SELECT is an error, not
/// JSON with the label ignored — the failure that made `ikigai-sparql`'s declared
/// outputs true by accident before it was fixed there.
#[test]
fn an_as_the_query_form_cannot_answer_in_is_refused() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ, CAP_WRITE]);
    load(&k, &c, "<http://ex/a> <http://ex/p> <http://ex/b> .").unwrap();

    let err = block_on(
        k.issue(
            Request::new(Verb::Source, iri("urn:iki:store:select"))
                .with_arg("query", ArgRef::Inline(b"SELECT * { ?s ?p ?o }".to_vec()))
                .with_arg("as", ArgRef::Inline(b"text/turtle".to_vec())),
            &c,
        ),
    )
    .unwrap_err();
    assert!(
        matches!(&err, ikigai_core::Error::InvalidArgument { name, .. } if name == "as"),
        "got {err:?}"
    );

    // And the selector really selects, within the family the form can answer in.
    let rep = block_on(
        k.issue(
            Request::new(Verb::Source, iri("urn:iki:store:construct"))
                .with_arg(
                    "query",
                    ArgRef::Inline(b"CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }".to_vec()),
                )
                .with_arg("as", ArgRef::Inline(b"text/turtle".to_vec())),
            &c,
        ),
    )
    .unwrap();
    assert_eq!(rep.repr_type.media_type, "text/turtle");

    let rep = block_on(
        k.issue(
            Request::new(Verb::Source, iri("urn:iki:store:select"))
                .with_arg("query", ArgRef::Inline(b"SELECT * { ?s ?p ?o }".to_vec()))
                .with_arg("as", ArgRef::Inline(b"text/csv".to_vec())),
            &c,
        ),
    )
    .unwrap();
    assert_eq!(rep.repr_type.media_type, "text/csv");
}

/// A load into a named graph lands there, and a bad `graph=` is an argument error rather
/// than a silent default-graph load.
#[test]
fn load_honours_the_graph_argument_and_refuses_a_non_iri() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ, CAP_WRITE]);

    block_on(
        k.issue(
            Request::new(Verb::Sink, iri("urn:iki:store:load"))
                .with_arg(
                    "content",
                    ArgRef::Inline(b"<http://ex/a> <http://ex/p> <http://ex/b> .".to_vec()),
                )
                .with_arg("graph", ArgRef::Inline(b"http://ex/g".to_vec())),
            &c,
        ),
    )
    .unwrap();
    let json = select(
        &k,
        &c,
        "SELECT ?s WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }",
    )
    .unwrap();
    assert!(json.contains("http://ex/a"), "{json}");

    let err = block_on(
        k.issue(
            Request::new(Verb::Sink, iri("urn:iki:store:load"))
                .with_arg(
                    "content",
                    ArgRef::Inline(b"<http://ex/a> <http://ex/p> <http://ex/b> .".to_vec()),
                )
                .with_arg("graph", ArgRef::Inline(b"not an iri".to_vec())),
            &c,
        ),
    )
    .unwrap_err();
    assert!(
        matches!(&err, ikigai_core::Error::InvalidArgument { name, .. } if name == "graph"),
        "got {err:?}"
    );
}

/// Garbage in `content` is an `InvalidArgument` naming the argument, not an opaque
/// endpoint error — a caller (or an agent) can tell "I sent the wrong thing" from "the
/// store broke".
#[test]
fn malformed_input_names_the_argument_that_was_wrong() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ, CAP_WRITE]);

    let err = load(&k, &c, "this is not turtle {{{").unwrap_err();
    assert!(
        matches!(&err, ikigai_core::Error::InvalidArgument { name, .. } if name == "content"),
        "got {err:?}"
    );
    let err = select(&k, &c, "SELEKT * { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(&err, ikigai_core::Error::InvalidArgument { name, .. } if name == "query"),
        "got {err:?}"
    );
}

/// ★ The IRI is a promise about the result shape, so a query of another form is REFUSED
/// rather than served. Without this, `urn:iki:store:select` could answer with a graph and
/// its declared `outputs` would be true only by accident — the same failure `OUTPUTS`
/// caught in `ikigai-sparql` when `sparql-construct` served Turtle while declaring JSON.
#[test]
fn a_query_of_the_wrong_form_is_refused_by_the_iri_it_was_sent_to() {
    let k = kernel(DurableStore::in_memory().unwrap());
    let c = cap(&[CAP_READ]);

    let err = ask_form(
        &k,
        &c,
        "select",
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
    )
    .unwrap_err();
    assert!(
        matches!(&err, ikigai_core::Error::InvalidArgument { name, detail }
                 if name == "query" && detail.contains("a result set")),
        "got {err:?}"
    );

    let err = ask_form(&k, &c, "construct", "SELECT * { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(&err, ikigai_core::Error::InvalidArgument { name, detail }
                 if name == "query" && detail.contains("a graph")),
        "got {err:?}"
    );

    // ASK and SELECT share a result FAMILY, so they are interchangeable by shape — the
    // check is about what comes back, not about the keyword. Stated here because a
    // reader will otherwise expect a refusal.
    assert!(ask_form(&k, &c, "ask", "SELECT * { ?s ?p ?o }").is_ok());
}
