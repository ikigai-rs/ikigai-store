//! `urn:cap:store:read:graph:<iri>` — declared and enforced, ablated both directions.
//!
//! # What these tests are for
//!
//! 0.2.1 segmented WRITES and left [`CAP_READ`](ikigai_store::CAP_READ) as the whole
//! dataset, which left the tenancy boundary with a documented bypass: a module enforcing
//! its own read capability over the graph it owns could be gone around entirely by
//! querying the store directly under the broad grant. These tests are the evidence that
//! the read half now closes, and they are the ONLY evidence — `ikigai-conformance`'s
//! `AUTHORITY` check is silent on a cap-declaring Source, and no check anywhere can see
//! the *parameterized* half of a wildcard scope. The interesting case is always a caller
//! holding a grant for one graph reaching for another, and only a test written on purpose
//! looks at it.
//!
//! # ★ Every test is a PAIR, and that is the whole design
//!
//! **Empty is a legitimate answer to a query.** A confinement that is wrong in the safe
//! direction — one that returns nothing when it should return rows — is indistinguishable
//! from an empty graph, so a suite of refusals alone would pass over a read face that had
//! quietly stopped working. Each test below therefore pins the *positive* case (the
//! permitted graph comes back, with its rows) beside the refusal.
//!
//! The mechanism, and the per-query-shape probes behind it, are in `src/scope.rs`.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, space, DurableStore, CAP_READ, CAP_READ_GRAPH, CAP_WRITE,
    CAP_WRITE_GRAPH,
};
use std::sync::Arc;

const G: &str = "urn:example:acme";
const OTHER: &str = "urn:example:zenith";

/// Two tenants' graphs and one triple in the store's own default graph. The third is what
/// makes "a scoped reader cannot reach the default graph" testable rather than asserted.
fn kernel() -> Kernel {
    let store = DurableStore::in_memory().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    let seed = format!(
        "INSERT DATA {{ \
           GRAPH <{G}> {{ <urn:example:acme:1> <urn:p> \"acme\" }} \
           GRAPH <{OTHER}> {{ <urn:example:zenith:1> <urn:p> \"zenith\" }} \
           <urn:example:host:1> <urn:p> \"host\" \
         }}"
    );
    sink(
        &kernel,
        "urn:iki:store:update",
        &[("content", &seed)],
        &Capability::root(),
    )
    .expect("seeding");
    kernel
}

fn sink(
    kernel: &Kernel,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    issue(kernel, Verb::Sink, iri, args, cap)
}

fn source(
    kernel: &Kernel,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    issue(kernel, Verb::Source, iri, args, cap)
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

/// A scoped SELECT over `G`, as CSV so a row count is a line count.
fn scoped_select(kernel: &Kernel, query: &str, cap: &Capability) -> Result<Vec<String>, Error> {
    source(
        kernel,
        "urn:iki:store:graph-select",
        &[("graph", G), ("query", query), ("as", "text/csv")],
        cap,
    )
    .map(|csv| csv.lines().skip(1).map(str::to_string).collect())
}

/// The control: the same query through the broad door under root, so every "it is
/// confined" claim below is a DIFFERENCE and not a query that matches nothing anyway.
fn broad_select(kernel: &Kernel, query: &str) -> Vec<String> {
    source(
        kernel,
        "urn:iki:store:select",
        &[("query", query), ("as", "text/csv")],
        &Capability::root(),
    )
    .expect("the broad door under root")
    .lines()
    .skip(1)
    .map(str::to_string)
    .collect()
}

/// Exactly the grant a host would hand a module that reads one graph.
fn one_graph() -> Capability {
    Capability::scoped([cap_read_graph(G)])
}

// ------------------------------------------------------------------ the grant works

/// ★ The positive case for the commonest shape. If the confinement were wrong in the safe
/// direction this is the only test that would notice.
#[test]
fn a_graph_grant_reads_its_own_graph() {
    let kernel = kernel();
    let rows = scoped_select(&kernel, "SELECT ?s WHERE { ?s ?p ?o }", &one_graph())
        .expect("an in-scope read under the graph grant");
    assert_eq!(rows, ["urn:example:acme:1"]);
}

/// `graph=G` means `FROM <G> FROM NAMED <G>`, so the graph is reachable **both** ways —
/// as the default graph and by name. A caller should not have to know which.
#[test]
fn the_scoped_graph_is_reachable_as_the_default_graph_and_by_name() {
    let kernel = kernel();
    for query in [
        "SELECT ?s WHERE { ?s ?p ?o }".to_string(),
        format!("SELECT ?s WHERE {{ GRAPH <{G}> {{ ?s ?p ?o }} }}"),
        "SELECT ?s WHERE { GRAPH ?g { ?s ?p ?o } }".to_string(),
    ] {
        assert_eq!(
            scoped_select(&kernel, &query, &one_graph()).unwrap_or_else(|e| panic!("{query}: {e}")),
            ["urn:example:acme:1"],
            "`{query}` did not read the permitted graph"
        );
    }
}

/// Each of the four forms answers, scoped — the ablations below are worth nothing if the
/// face itself does not work.
#[test]
fn all_four_scoped_forms_answer_over_the_permitted_graph() {
    let kernel = kernel();
    let cap = one_graph();
    let ask = source(
        &kernel,
        "urn:iki:store:graph-ask",
        &[("graph", G), ("query", "ASK { ?s ?p ?o }")],
        &cap,
    )
    .expect("a scoped ASK");
    assert!(ask.contains("true"), "{ask}");

    let construct = source(
        &kernel,
        "urn:iki:store:graph-construct",
        &[
            ("graph", G),
            (
                "query",
                "CONSTRUCT { ?s <urn:copied> ?o } WHERE { ?s ?p ?o }",
            ),
        ],
        &cap,
    )
    .expect("a scoped CONSTRUCT");
    assert!(construct.contains("urn:example:acme:1"), "{construct}");
    assert!(!construct.contains("zenith"), "{construct}");

    // ⚠ DESCRIBE collects from the dataset's DEFAULT graph only (upstream; pinned in
    // `src/scope.rs`), so the scoped door — where the default graph is `G` — is the one
    // where describing a tenant's subject works at all.
    let describe = source(
        &kernel,
        "urn:iki:store:graph-describe",
        &[("graph", G), ("query", "DESCRIBE <urn:example:acme:1>")],
        &cap,
    )
    .expect("a scoped DESCRIBE");
    assert!(describe.contains("\"acme\""), "{describe}");
}

/// ★ Several graph scopes at once is the intended use, not a workaround: a host
/// segmenting work by client grants one token per client.
#[test]
fn several_graph_grants_are_held_and_used_independently() {
    let kernel = kernel();
    let both = Capability::scoped([cap_read_graph(G), cap_read_graph(OTHER)]);
    for (graph, expect) in [(G, "urn:example:acme:1"), (OTHER, "urn:example:zenith:1")] {
        let rows: Vec<String> = source(
            &kernel,
            "urn:iki:store:graph-select",
            &[
                ("graph", graph),
                ("query", "SELECT ?s WHERE { ?s ?p ?o }"),
                ("as", "text/csv"),
            ],
            &both,
        )
        .unwrap_or_else(|e| panic!("reading {graph} with both grants: {e}"))
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect();
        assert_eq!(rows, [expect]);
    }
}

// ------------------------------------------------------------ the grant is a boundary

/// The capability ablation: the grant for one graph is refused for another, by the
/// endpoint, before the query is even parsed.
#[test]
fn a_graph_grant_is_refused_for_another_graph() {
    let kernel = kernel();
    let err = source(
        &kernel,
        "urn:iki:store:graph-select",
        &[("graph", OTHER), ("query", "SELECT ?s WHERE { ?s ?p ?o }")],
        &one_graph(),
    )
    .expect_err("a grant for one graph must not read another");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(err.to_string().contains(&cap_read_graph(OTHER)), "{err}");
}

/// ★ The confinement ablation, one case per shape that a syntactic check would miss.
/// Each is paired with the same query through the broad door, so the assertion is that
/// the scoped read saw LESS — not merely that it saw nothing.
#[test]
fn a_scoped_read_cannot_reach_another_graph_by_any_shape() {
    let kernel = kernel();
    for (name, query) in [
        (
            "another named graph, named outright",
            format!("SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"),
        ),
        (
            "a sub-select over another graph",
            format!(
                "SELECT ?s WHERE {{ {{ SELECT ?s WHERE {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }} }} }}"
            ),
        ),
        (
            "a FILTER EXISTS over another graph",
            format!(
                "SELECT ?s WHERE {{ ?s ?p ?o FILTER EXISTS {{ GRAPH <{OTHER}> {{ ?a ?b ?c }} }} }}"
            ),
        ),
        (
            "the store's own default graph, which no grant can name",
            "SELECT ?s WHERE { ?s ?p \"host\" }".to_string(),
        ),
    ] {
        let scoped = scoped_select(&kernel, &query, &one_graph())
            .unwrap_or_else(|e| panic!("{name}: the query is well-formed and in scope: {e}"));
        assert!(scoped.is_empty(), "{name}: leaked {scoped:?}");
        assert!(
            !broad_select(&kernel, &query).is_empty(),
            "{name}: the control found nothing either, so this test proves nothing"
        );
    }
}

/// A variable graph must ENUMERATE only the permitted set — a distinct failure from
/// matching nothing, and the one a syntactic check cannot see at all.
#[test]
fn a_variable_graph_enumerates_only_the_permitted_graph() {
    let kernel = kernel();
    let query = "SELECT ?g WHERE { GRAPH ?g { ?s ?p ?o } }";
    assert_eq!(
        scoped_select(&kernel, query, &one_graph()).expect("enumerating under the grant"),
        [G]
    );
    let mut broad = broad_select(&kernel, query);
    broad.sort();
    assert_eq!(
        broad,
        [G, OTHER],
        "the control: both graphs really are there"
    );
}

/// ★ `FROM` / `FROM NAMED` is a SECOND way to name a dataset. It cannot widen the scope —
/// `src/scope.rs` pins that at the mechanism level — but a `FROM <other>` answered with
/// this graph's rows would be a wrong answer that looked right, so it is refused.
#[test]
fn a_query_naming_its_own_dataset_is_refused() {
    let kernel = kernel();
    for query in [
        format!("SELECT ?s FROM <{OTHER}> WHERE {{ ?s ?p ?o }}"),
        format!("SELECT ?s FROM <{G}> WHERE {{ ?s ?p ?o }}"),
        format!("SELECT ?s FROM NAMED <{OTHER}> WHERE {{ GRAPH ?g {{ ?s ?p ?o }} }}"),
    ] {
        let err = scoped_select(&kernel, &query, &one_graph())
            .err()
            .unwrap_or_else(|| panic!("`{query}` should be refused"));
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "query"),
            "{query}: {err:?}"
        );
        assert!(err.to_string().contains("FROM"), "{err}");
    }
    // …and the broad door still accepts them, because there it names nothing it may not
    // already read.
    assert_eq!(
        broad_select(
            &kernel,
            &format!("SELECT ?s FROM <{OTHER}> WHERE {{ ?s ?p ?o }}")
        ),
        ["urn:example:zenith:1"]
    );
}

// -------------------------------------------------- the two scopes do not substitute

/// ★ Declared is enforced in BOTH directions, across the two read doors. This is the test
/// the whole arc exists for: the broad read grant must not open the narrow door (or the
/// declaration is a lie), and the narrow grant must not open the broad one (or the
/// boundary has the very bypass it was built to close).
#[test]
fn neither_read_scope_stands_in_for_the_other() {
    let kernel = kernel();
    let broad = Capability::scoped([CAP_READ.to_string()]);
    let narrow = one_graph();
    let query = "SELECT ?s WHERE { ?s ?p ?o }";

    let err = scoped_select(&kernel, query, &broad).expect_err("broad must not open the narrow");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    let err = source(
        &kernel,
        "urn:iki:store:select",
        &[("query", query)],
        &narrow,
    )
    .expect_err("narrow must not open the broad door — this IS the bypass");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    // Each opens its own.
    assert_eq!(
        scoped_select(&kernel, query, &narrow).expect("the narrow door under the narrow grant"),
        ["urn:example:acme:1"]
    );
    assert!(
        source(&kernel, "urn:iki:store:select", &[("query", query)], &broad).is_ok(),
        "the broad door under the broad grant"
    );
}

/// ★ Read and write scopes over the SAME graph are separate grants, and neither implies
/// the other. A module that appends to its ledger and never reads it holds one token.
#[test]
fn a_write_scope_does_not_imply_a_read_scope_over_the_same_graph() {
    let kernel = kernel();
    let write_only = Capability::scoped([cap_write_graph(G)]);
    let err = scoped_select(&kernel, "SELECT ?s WHERE { ?s ?p ?o }", &write_only)
        .expect_err("a write scope must not read");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    let read_only = one_graph();
    let err = sink(
        &kernel,
        "urn:iki:store:graph-update",
        &[
            ("graph", G),
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:x> <urn:p> \"no\" }} }}"),
            ),
        ],
        &read_only,
    )
    .expect_err("a read scope must not write");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");

    // Both together do both — which is what a host grants a module that owns a graph.
    let owner = Capability::scoped([cap_read_graph(G), cap_write_graph(G)]);
    sink(
        &kernel,
        "urn:iki:store:graph-update",
        &[
            ("graph", G),
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:x> <urn:p> \"yes\" }} }}"),
            ),
        ],
        &owner,
    )
    .expect("the owner writes");
    assert_eq!(
        scoped_select(&kernel, "SELECT ?s WHERE { ?s ?p ?o }", &owner)
            .expect("the owner reads")
            .len(),
        2
    );
}

#[test]
fn no_grants_at_all_opens_neither_read_door() {
    let kernel = kernel();
    let none = Capability::scoped(Vec::<String>::new());
    let query = "SELECT ?s WHERE { ?s ?p ?o }";
    assert!(matches!(
        scoped_select(&kernel, query, &none).unwrap_err(),
        Error::Denied(_)
    ));
    assert!(matches!(
        source(&kernel, "urn:iki:store:select", &[("query", query)], &none).unwrap_err(),
        Error::Denied(_)
    ));
}

/// The declared wildcard is the one the kernel pre-checks, so a capability holding the
/// wildcard TOKEN itself — rather than a grant under it — must still be refused by the
/// endpoint. Otherwise "declared = enforced" would be satisfied by the declaration alone.
#[test]
fn the_wildcard_token_itself_grants_nothing() {
    let kernel = kernel();
    let wildcard = Capability::scoped([CAP_READ_GRAPH.to_string()]);
    let err = scoped_select(&kernel, "SELECT ?s WHERE { ?s ?p ?o }", &wildcard)
        .expect_err("the wildcard is a declaration, not a grant over any graph");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
}

// ------------------------------------------------------------------ freshness + args

/// A scoped read is cacheable under the same three write threads as a broad one, so a
/// write through any door invalidates it. Without this the boundary would be correct and
/// the answers stale.
#[test]
fn a_scoped_read_is_invalidated_by_a_write() {
    let kernel = kernel();
    let cap = Capability::scoped([cap_read_graph(G), cap_write_graph(G)]);
    let read = || {
        source(
            &kernel,
            "urn:iki:store:graph-ask",
            &[
                ("graph", G),
                ("query", "ASK { <urn:example:acme:2> ?p ?o }"),
            ],
            &cap,
        )
        .expect("the scoped ASK")
    };
    assert!(read().contains("false"), "the subject is not there yet");
    sink(
        &kernel,
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
        &cap,
    )
    .expect("the scoped write");
    assert!(
        read().contains("true"),
        "a cached scoped read was not invalidated by a write"
    );
}

/// ⚠ The cache keys on the capability fingerprint, so two tenants' identical queries
/// cannot serve each other's bytes. Stated as a test because a cache that ignored the
/// capability would be a silent cross-tenant read with every ablation above still green.
#[test]
fn two_tenants_running_the_same_query_do_not_share_a_cached_answer() {
    let kernel = kernel();
    let query = "SELECT ?s WHERE { ?s ?p ?o }";
    let acme = Capability::scoped([cap_read_graph(G)]);
    let zenith = Capability::scoped([cap_read_graph(OTHER)]);
    let rows = |graph: &str, cap: &Capability| -> Vec<String> {
        source(
            &kernel,
            "urn:iki:store:graph-select",
            &[("graph", graph), ("query", query), ("as", "text/csv")],
            cap,
        )
        .expect("a scoped read")
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
    };
    assert_eq!(rows(G, &acme), ["urn:example:acme:1"]);
    assert_eq!(rows(OTHER, &zenith), ["urn:example:zenith:1"]);
    // …and again, now that both are cached.
    assert_eq!(rows(G, &acme), ["urn:example:acme:1"]);
    assert_eq!(rows(OTHER, &zenith), ["urn:example:zenith:1"]);
}

#[test]
fn a_graph_that_is_not_an_iri_is_refused_by_name() {
    let kernel = kernel();
    let err = source(
        &kernel,
        "urn:iki:store:graph-select",
        &[
            ("graph", "not an iri"),
            ("query", "SELECT ?s WHERE { ?s ?p ?o }"),
        ],
        &Capability::root(),
    )
    .expect_err("a malformed graph IRI");
    assert!(
        matches!(&err, Error::InvalidArgument { name, .. } if name == "graph"),
        "{err:?}"
    );
}

/// The scoped forms keep the wrong-shape refusal, and the message names the scoped IRI
/// rather than the broad one it was copied from.
#[test]
fn a_query_of_the_wrong_form_is_refused_under_the_scoped_iri_too() {
    let kernel = kernel();
    let err = scoped_select(
        &kernel,
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
        &one_graph(),
    )
    .expect_err("a CONSTRUCT under the scoped SELECT IRI");
    let text = err.to_string();
    assert!(text.contains("urn:iki:store:graph-select"), "{text}");
    assert!(text.contains("a result set"), "{text}");
}

/// `bindings=` works on the scoped forms too — a tenant's own values still get into a
/// query without becoming syntax.
#[test]
fn bindings_reach_the_scoped_forms() {
    let kernel = kernel();
    let rows = source(
        &kernel,
        "urn:iki:store:graph-select",
        &[
            ("graph", G),
            ("query", "SELECT * WHERE { ?s ?p ?o }"),
            ("bindings", r#"{"o": "acme"}"#),
            ("as", "text/csv"),
        ],
        &one_graph(),
    )
    .expect("a bound scoped read");
    assert!(rows.contains("urn:example:acme:1"), "{rows}");
}

/// The two capability CONSTANTS are the declarations, and they must not collide with the
/// write ones: a host reading the manifold distinguishes the doors by exactly this.
#[test]
fn the_declared_scopes_are_distinct() {
    assert_ne!(CAP_READ_GRAPH, CAP_WRITE_GRAPH);
    assert_ne!(cap_read_graph(G), cap_write_graph(G));
    assert_ne!(cap_read_graph(G), CAP_READ);
    assert_ne!(cap_write_graph(G), CAP_WRITE);
}
