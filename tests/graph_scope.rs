//! `urn:cap:store:write:graph:<iri>` — declared and enforced, ablated both directions.
//!
//! # What these tests are for
//!
//! `urn:cap:store:write` is `DROP ALL`, so a module layered over this store makes its
//! callers hold that authority to append one triple. The per-graph scope is the boundary
//! that fixes it, and a boundary is only worth what its ablation tests prove — so every
//! test here is a pair: the thing the grant *does* allow, and the thing it does not.
//!
//! **The escape cases are the point.** A scope checked against an update's *syntax* would
//! be defeated by `DELETE WHERE { GRAPH ?g { … } }` (the graph is a variable) and by
//! `INSERT DATA { <s> <p> <o> }` (no graph is named, and the default graph is not the
//! scoped one). This scope is checked on *effects*, so each of those is a test below
//! rather than a caveat in a doc comment. See `src/confine.rs` for the mechanism.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_write_graph, space, DurableStore, CAP_READ, CAP_WRITE};
use std::sync::Arc;

const SCOPED: &str = "urn:iki:store:graph-update";
const BROAD: &str = "urn:iki:store:update";
const G: &str = "urn:example:acme";
const OTHER: &str = "urn:example:zenith";

fn kernel() -> Kernel {
    let store = DurableStore::in_memory().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    // Two tenants' graphs, seeded through the broad door under root.
    for (graph, subject) in [(G, "urn:example:acme:1"), (OTHER, "urn:example:zenith:1")] {
        sink(
            &kernel,
            BROAD,
            &[(
                "content",
                &format!("INSERT DATA {{ GRAPH <{graph}> {{ <{subject}> <urn:p> \"seed\" }} }}"),
            )],
            &Capability::root(),
        )
        .expect("seeding");
    }
    kernel
}

fn sink(
    kernel: &Kernel,
    iri: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    let mut request = Request::new(Verb::Sink, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, cap)).map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

fn scoped(kernel: &Kernel, update: &str, cap: &Capability) -> Result<String, Error> {
    sink(kernel, SCOPED, &[("graph", G), ("content", update)], cap)
}

/// Exactly the grant a host would hand a module that owns one graph.
fn one_graph() -> Capability {
    Capability::scoped([cap_write_graph(G), CAP_READ.to_string()])
}

fn quads(kernel: &Kernel, graph: &str) -> usize {
    let request = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
        .with_arg(
            "query",
            ArgRef::Inline(
                format!("SELECT * WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}").into_bytes(),
            ),
        )
        .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()));
    let repr = block_on(kernel.issue(request, &Capability::root())).expect("counting");
    // CSV: one header line, then one line per row.
    String::from_utf8_lossy(&repr.bytes).lines().count() - 1
}

// ----------------------------------------------------------------- the grant works

#[test]
fn a_graph_grant_writes_its_own_graph() {
    let kernel = kernel();
    let out = scoped(
        &kernel,
        &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:example:acme:2> <urn:p> \"new\" }} }}"),
        &one_graph(),
    )
    .expect("an in-scope write under the graph grant");
    assert!(out.contains("+1 -0"), "{out}");
    assert_eq!(quads(&kernel, G), 2);
    assert_eq!(quads(&kernel, OTHER), 1);
}

#[test]
fn a_graph_grant_deletes_and_inserts_atomically_in_its_own_graph() {
    let kernel = kernel();
    scoped(
        &kernel,
        &format!(
            "DELETE {{ GRAPH <{G}> {{ ?s <urn:p> \"seed\" }} }} \
             INSERT {{ GRAPH <{G}> {{ ?s <urn:p> \"closed\" }} }} \
             WHERE  {{ GRAPH <{G}> {{ ?s <urn:p> \"seed\" }} }}"
        ),
        &one_graph(),
    )
    .expect("a delete+insert within scope");
    assert_eq!(quads(&kernel, G), 1);
}

/// ★ Several graph scopes at once is the intended use, not a workaround: a host
/// segmenting work by client grants one token per client, and a caller holding two
/// writes both — separately, because the boundary is per request.
#[test]
fn several_graph_grants_are_held_and_used_independently() {
    let kernel = kernel();
    let both = Capability::scoped([
        cap_write_graph(G),
        cap_write_graph(OTHER),
        CAP_READ.to_string(),
    ]);
    for graph in [G, OTHER] {
        sink(
            &kernel,
            SCOPED,
            &[
                ("graph", graph),
                (
                    "content",
                    &format!(
                        "INSERT DATA {{ GRAPH <{graph}> {{ <urn:example:x> <urn:p> \"2\" }} }}"
                    ),
                ),
            ],
            &both,
        )
        .unwrap_or_else(|e| panic!("writing {graph} with both grants: {e}"));
    }
    assert_eq!(quads(&kernel, G), 2);
    assert_eq!(quads(&kernel, OTHER), 2);
}

// ------------------------------------------------------------ the grant is a boundary

/// The capability ablation: the grant for one graph is refused for another, by the
/// endpoint, before anything is evaluated.
#[test]
fn a_graph_grant_is_refused_for_another_graph() {
    let kernel = kernel();
    let err = sink(
        &kernel,
        SCOPED,
        &[
            ("graph", OTHER),
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{OTHER}> {{ <urn:example:x> <urn:p> \"no\" }} }}"),
            ),
        ],
        &one_graph(),
    )
    .expect_err("a grant for one graph must not write another");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(err.to_string().contains(&cap_write_graph(OTHER)), "{err}");
    assert_eq!(quads(&kernel, OTHER), 1);
}

/// ★ The effects ablation, one test per shape that defeats a syntactic check. Each must
/// be refused IN FULL — the counts afterwards are as load-bearing as the error.
#[test]
fn an_update_that_reaches_outside_the_graph_is_refused_in_full() {
    for (name, update) in [
        (
            "another named graph",
            format!("INSERT DATA {{ GRAPH <{OTHER}> {{ <urn:example:x> <urn:p> \"no\" }} }}"),
        ),
        (
            "the default graph, named by nothing at all",
            "INSERT DATA { <urn:example:x> <urn:p> \"no\" }".to_string(),
        ),
        (
            "WITH, which names the graph outside the block",
            format!("WITH <{OTHER}> INSERT {{ <urn:example:x> <urn:p> \"no\" }} WHERE {{}}"),
        ),
        (
            "COPY, whose destination is the escape",
            format!("COPY <{G}> TO <{OTHER}>"),
        ),
        (
            "MOVE, which is COPY plus a drop",
            format!("MOVE <{G}> TO <{OTHER}>"),
        ),
        (
            "CREATE GRAPH, which writes no quad but registers a graph",
            format!("CREATE GRAPH <{OTHER}>"),
        ),
        (
            "a second operation in a `;` chain",
            format!(
                "INSERT DATA {{ GRAPH <{G}> {{ <urn:example:acme:2> <urn:p> \"ok\" }} }}; \
                 INSERT DATA {{ GRAPH <{OTHER}> {{ <urn:example:x> <urn:p> \"no\" }} }}"
            ),
        ),
    ] {
        let kernel = kernel();
        let err = match scoped(&kernel, &update, &one_graph()) {
            Ok(out) => panic!("{name}: expected a refusal, got `{out}`"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Denied(_)), "{name}: {err:?}");
        // ★ Nothing was applied — including the in-scope half of the `;` chain, which is
        // what makes a refusal here safe to retry.
        assert_eq!(quads(&kernel, G), 1, "{name} left the scoped graph changed");
        assert_eq!(quads(&kernel, OTHER), 1, "{name} reached another graph");
    }
}

/// ★ The two shapes a syntactic check cannot see at all: a graph named by a VARIABLE,
/// and a whole-dataset operation. Both are *allowed*, and both are confined to `G` — the
/// private dataset held only `G`, and a grant over `G` includes emptying it.
#[test]
fn a_variable_graph_and_drop_all_are_confined_rather_than_refused() {
    for update in [
        "DELETE WHERE { GRAPH ?g { ?s ?p ?o } }",
        "DROP ALL",
        "CLEAR ALL",
    ] {
        let kernel = kernel();
        scoped(&kernel, update, &one_graph())
            .unwrap_or_else(|e| panic!("`{update}` should be confined, not refused: {e}"));
        assert_eq!(
            quads(&kernel, G),
            0,
            "`{update}` did not empty its own graph"
        );
        assert_eq!(
            quads(&kernel, OTHER),
            1,
            "`{update}` reached another tenant's graph"
        );
    }
}

/// ⚠ The read half of the boundary, stated as a test because it is a behaviour
/// difference and not only a security property: the scoped door sees `G` and nothing
/// else, so a WHERE clause reading another graph matches nothing.
#[test]
fn a_scoped_update_cannot_read_another_graph() {
    let kernel = kernel();
    scoped(
        &kernel,
        &format!(
            "INSERT {{ GRAPH <{G}> {{ ?s <urn:copied> \"leak\" }} }} \
             WHERE  {{ GRAPH <{OTHER}> {{ ?s ?p ?o }} }}"
        ),
        &one_graph(),
    )
    .expect("the update itself is well-formed and in scope");
    // It matched nothing, so nothing was copied: the other tenant's subjects never
    // reached this graph.
    assert_eq!(quads(&kernel, G), 1);
}

// --------------------------------------------------- the two scopes do not substitute

/// Declared is enforced in both directions, across the two write doors.
#[test]
fn neither_write_scope_stands_in_for_the_other() {
    let broad = Capability::scoped([CAP_WRITE.to_string(), CAP_READ.to_string()]);
    let narrow = one_graph();
    let insert = format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:example:acme:2> <urn:p> \"x\" }} }}");

    // The broad grant does not open the narrow door — it is not a superset token, and
    // saying so here is what keeps the declaration honest.
    let k = kernel();
    let err = scoped(&k, &insert, &broad).expect_err("broad must not open the narrow door");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert_eq!(quads(&k, G), 1);

    // And the narrow grant does not open the broad one — the kernel refuses it before
    // `invoke`, on the declared `requires`.
    let k = kernel();
    let err = sink(&k, BROAD, &[("content", &insert)], &narrow)
        .expect_err("narrow must not open the broad door");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert_eq!(quads(&k, G), 1);

    // Each opens its own.
    let k = kernel();
    scoped(&k, &insert, &narrow).expect("the narrow door under the narrow grant");
    assert_eq!(quads(&k, G), 2);
    let k = kernel();
    sink(&k, BROAD, &[("content", &insert)], &broad).expect("the broad door under the broad grant");
    assert_eq!(quads(&k, G), 2);
}

#[test]
fn no_grants_at_all_opens_neither_door() {
    let kernel = kernel();
    let none = Capability::scoped(Vec::<String>::new());
    let insert = format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:example:acme:2> <urn:p> \"x\" }} }}");
    assert!(matches!(
        scoped(&kernel, &insert, &none).unwrap_err(),
        Error::Denied(_)
    ));
    assert!(matches!(
        sink(&kernel, BROAD, &[("content", &insert)], &none).unwrap_err(),
        Error::Denied(_)
    ));
}

// ------------------------------------------------------------------- freshness + args

/// ★ A third writing IRI means a third golden thread, and a reader must depend on it.
/// Without this the scoped door would have left every cacheable read serving stale bytes
/// after a scoped write — silently, on the branch that looks like success.
#[test]
fn a_scoped_write_invalidates_a_cached_read() {
    let kernel = kernel();
    let read = || {
        let request = Request::new(Verb::Source, Iri::parse("urn:iki:store:ask").unwrap())
            .with_arg(
                "query",
                ArgRef::Inline(
                    format!("ASK {{ GRAPH <{G}> {{ <urn:example:acme:2> ?p ?o }} }}").into_bytes(),
                ),
            );
        String::from_utf8_lossy(
            &block_on(kernel.issue(request, &Capability::root()))
                .unwrap()
                .bytes,
        )
        .into_owned()
    };
    assert!(read().contains("false"), "the subject is not there yet");
    scoped(
        &kernel,
        &format!("INSERT DATA {{ GRAPH <{G}> {{ <urn:example:acme:2> <urn:p> \"new\" }} }}"),
        &one_graph(),
    )
    .expect("the scoped write");
    assert!(
        read().contains("true"),
        "the cached ASK was not invalidated by a scoped write — the read depends on \
         GRAPH_UPDATE_THREAD, or it does not"
    );
}

#[test]
fn a_graph_that_is_not_an_iri_is_refused_by_name() {
    let kernel = kernel();
    let err = sink(
        &kernel,
        SCOPED,
        &[("graph", "not an iri"), ("content", "DROP ALL")],
        &Capability::root(),
    )
    .expect_err("a malformed graph IRI");
    assert!(
        matches!(&err, Error::InvalidArgument { name, .. } if name == "graph"),
        "{err:?}"
    );
}

/// The refusal names the graph the update reached for, because that is how an operator
/// finds out what the update actually did.
#[test]
fn the_refusal_names_the_offending_statement() {
    let kernel = kernel();
    let err = scoped(
        &kernel,
        "INSERT DATA { <urn:example:x> <urn:p> \"stray\" }",
        &one_graph(),
    )
    .expect_err("a write to the default graph");
    let text = err.to_string();
    assert!(text.contains("DEFAULT graph"), "{text}");
    assert!(text.contains("urn:example:x"), "{text}");
    assert!(text.contains("Nothing was applied"), "{text}");
}
