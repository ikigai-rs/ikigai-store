//! A scoped read over SEVERAL named graphs — ledger #380.
//!
//! # What was wrong
//!
//! `graph=G` confined a query to exactly one graph, so a caller holding the grants for two
//! graphs could run two queries and never the join between them — and the join it did
//! write came back EMPTY, with nothing to say the dataset had been narrowed underneath it.
//! `ikigai-gonk` pinned that as `a_scoped_token_reads_browse_and_still_cannot_join`.
//!
//! # What these tests hold the fix to
//!
//! - **The join answers** when every graph is granted, through a bare pattern (the merged
//!   default graph) and through `GRAPH` blocks.
//! - **One missing grant refuses the whole read**, and the refusal names the missing token.
//!   ★ Never "answered over the graphs you do hold" — that is the silent narrowing this
//!   arc exists to close, and the easiest thing here to get backwards.
//! - **Nothing reaches outside the set**, the store's default graph included.
//! - **Order and repetition are one dataset** — the same answer, and the same cache entry.
//! - **Coverage is all-or-nothing** across the set.
//!
//! The per-shape probes of the mechanism (property paths, sub-selects, `SERVICE`) live
//! beside it in `src/scope.rs`; these go through the kernel door with real capabilities.
//! Every refusal is paired with a positive case, for the reason `tests/read_scope.rs`
//! states: empty is a legitimate answer, so a confinement wrong in the safe direction is
//! otherwise invisible.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Scope, Space, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, space, DurableStore, SharerWrites, Store, CAP_READ,
    CAP_READ_GRAPH,
};
use oxigraph::io::RdfFormat;
use std::sync::Arc;

/// The ledger's graph, in the story #380 tells.
const LEDGER: &str = "urn:example:ledger";
/// The browse graph the ledger joins against.
const BROWSE: &str = "urn:example:browse";
/// A third tenant, outside every set named here.
const THIRD: &str = "urn:example:third";

/// An item in LEDGER that points at a note in BROWSE, a quad in THIRD, and one in the
/// store's own default graph — as N-Quads, so the same bytes seed an owned store through
/// its Sink and a shared one through its handle.
fn corpus() -> String {
    format!(
        "<urn:item:1> <urn:annotated-by> <urn:note:1> <{LEDGER}> .\n\
         <urn:note:1> <urn:body> \"joined\" <{BROWSE}> .\n\
         <urn:secret:1> <urn:body> \"third\" <{THIRD}> .\n\
         <urn:host:1> <urn:body> \"host\" .\n"
    )
}

const JOIN: &str =
    "SELECT ?item ?body WHERE { ?item <urn:annotated-by> ?note . ?note <urn:body> ?body }";

fn owned() -> Kernel {
    let kernel = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", &corpus()), ("format", "application/n-quads")],
        &Capability::root(),
    )
    .expect("seeding");
    kernel
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

/// A scoped SELECT over `graphs`, as CSV rows without the header.
fn select(
    kernel: &Kernel,
    graphs: &str,
    query: &str,
    cap: &Capability,
) -> Result<Vec<String>, Error> {
    issue(
        kernel,
        Verb::Source,
        "urn:iki:store:graph-select",
        &[("graph", graphs), ("query", query), ("as", "text/csv")],
        cap,
    )
    .map(|csv| csv.lines().skip(1).map(str::to_string).collect())
}

fn grants(graphs: &[&str]) -> Capability {
    Capability::scoped(graphs.iter().map(|g| cap_read_graph(g)))
}

fn both() -> String {
    format!("{LEDGER} {BROWSE}")
}

// ------------------------------------------------------------------------ the join

/// ★★ The proof #380 asked for: two graphs, both tokens held, joined rows.
#[test]
fn holding_both_grants_joins_across_both_graphs() {
    let kernel = owned();
    let cap = grants(&[LEDGER, BROWSE]);
    assert_eq!(
        select(&kernel, &both(), JOIN, &cap).expect("the join under both grants"),
        ["urn:item:1,joined"],
        "a bare pattern joins across the merged default graph"
    );
    let named = format!(
        "SELECT ?item ?body WHERE {{ GRAPH <{LEDGER}> {{ ?item <urn:annotated-by> ?note }} \
         GRAPH <{BROWSE}> {{ ?note <urn:body> ?body }} }}"
    );
    assert_eq!(
        select(&kernel, &both(), &named, &cap).expect("the join by name"),
        ["urn:item:1,joined"],
        "GRAPH blocks join across the set"
    );
    // The control, and the bug as it was: one graph, the same query, nothing.
    assert!(select(&kernel, LEDGER, &named, &cap).unwrap().is_empty());
}

/// ★★ The rule most likely to be gotten backwards. One grant held, two graphs named: the
/// read is REFUSED and the refusal names the grant that is missing — it is not answered
/// over LEDGER alone, which would return an empty join that looks like "no annotations".
#[test]
fn one_missing_grant_refuses_the_read_and_names_the_missing_token() {
    let kernel = owned();
    let cap = grants(&[LEDGER]);
    for spelling in [
        both(),
        format!("{BROWSE} {LEDGER}"),
        format!("{BROWSE}\n{LEDGER}\n"),
    ] {
        let err =
            select(&kernel, &spelling, JOIN, &cap).expect_err("a read naming an ungranted graph");
        let Error::Denied(message) = &err else {
            panic!("`{spelling}`: expected a typed Denied, got {err:?}");
        };
        assert!(
            message.contains(&format!("`{}`", cap_read_graph(BROWSE))),
            "the refusal must name the missing token: {message}"
        );
        assert!(
            !message.contains(&format!("`{}`", cap_read_graph(LEDGER))),
            "the refusal named a grant the caller holds as missing: {message}"
        );
    }
    // The positive half of the pair: the grant it holds still reads, alone.
    assert_eq!(
        select(&kernel, LEDGER, "SELECT ?s WHERE { ?s ?p ?o }", &cap).unwrap(),
        ["urn:item:1"]
    );
}

#[test]
fn every_missing_grant_is_named_not_only_the_first() {
    let kernel = owned();
    let err = select(&kernel, &both(), JOIN, &grants(&[THIRD])).expect_err("neither granted");
    let Error::Denied(message) = &err else {
        panic!("expected a typed Denied, got {err:?}");
    };
    for graph in [LEDGER, BROWSE] {
        assert!(
            message.contains(&cap_read_graph(graph)),
            "{graph} unnamed: {message}"
        );
    }
}

/// A comma is legal inside an IRI, so `A,B` is one graph nobody holds a grant for — and
/// the refusal says what was probably meant instead of only that it failed.
#[test]
fn a_comma_separated_list_is_refused_with_the_separator_named() {
    let kernel = owned();
    let err = select(
        &kernel,
        &format!("{LEDGER},{BROWSE}"),
        JOIN,
        &grants(&[LEDGER, BROWSE]),
    )
    .expect_err("a comma is not a separator");
    let Error::Denied(message) = &err else {
        panic!("expected a typed Denied, got {err:?}");
    };
    assert!(message.contains("whitespace"), "{message}");
}

#[test]
fn an_empty_graph_list_is_refused_by_name() {
    let kernel = owned();
    for raw in ["", "  \n"] {
        let err = select(&kernel, raw, JOIN, &grants(&[LEDGER])).expect_err("an empty list");
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "graph"),
            "`{raw:?}`: {err:?}"
        );
    }
}

// ----------------------------------------------------------------- the boundary

/// Rule 3 through the kernel: a set of two reaches neither the third tenant nor the
/// store's default graph, by name or by pattern — beside the positive case in one query.
#[test]
fn a_set_reaches_nothing_outside_itself() {
    let kernel = owned();
    let cap = grants(&[LEDGER, BROWSE]);
    for query in [
        format!("SELECT ?s WHERE {{ GRAPH <{THIRD}> {{ ?s ?p ?o }} }}"),
        "SELECT ?s WHERE { ?s <urn:body> \"third\" }".to_string(),
        "SELECT ?s WHERE { ?s <urn:body> \"host\" }".to_string(),
    ] {
        assert!(
            select(&kernel, &both(), &query, &cap).unwrap().is_empty(),
            "`{query}` reached outside the set"
        );
    }
    assert_eq!(
        select(
            &kernel,
            &both(),
            "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } } ORDER BY ?g",
            &cap
        )
        .unwrap(),
        [BROWSE, LEDGER],
        "GRAPH ?g binds exactly the set"
    );
    // The control: the broad door under root does see THIRD and the default graph, so the
    // empties above are the confinement and not an empty store.
    let broad = issue(
        &kernel,
        Verb::Source,
        "urn:iki:store:select",
        &[
            (
                "query",
                "SELECT ?s WHERE { { GRAPH ?g { ?s <urn:body> ?b } } UNION { ?s <urn:body> ?b } }",
            ),
            ("as", "text/csv"),
        ],
        &Capability::scoped([CAP_READ.to_string()]),
    )
    .unwrap();
    assert!(
        broad.contains("urn:secret:1") && broad.contains("urn:host:1"),
        "{broad}"
    );
}

/// Naming a graph a caller DOES hold a grant for but did not put in `graph=` is not a way
/// around the set either: the grant is authority, the argument is the dataset.
#[test]
fn a_held_grant_outside_the_named_set_does_not_widen_it() {
    let kernel = owned();
    let cap = grants(&[LEDGER, BROWSE, THIRD]);
    assert!(select(
        &kernel,
        &both(),
        &format!("SELECT ?s WHERE {{ GRAPH <{THIRD}> {{ ?s ?p ?o }} }}"),
        &cap
    )
    .unwrap()
    .is_empty());
}

#[test]
fn a_from_clause_is_still_refused_over_a_set() {
    let kernel = owned();
    let err = select(
        &kernel,
        &both(),
        &format!("SELECT ?s FROM <{THIRD}> WHERE {{ ?s ?p ?o }}"),
        &grants(&[LEDGER, BROWSE]),
    )
    .expect_err("a FROM clause");
    let Error::InvalidArgument { name, detail } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(name, "query");
    assert!(
        detail.contains(&format!("FROM NAMED <{BROWSE}> FROM NAMED <{LEDGER}>")),
        "the refusal states the dataset `graph=` built: {detail}"
    );
}

/// All four scoped forms take a set; each answers with something only the join can give.
#[test]
fn all_four_scoped_forms_read_the_whole_set() {
    let kernel = owned();
    let cap = grants(&[LEDGER, BROWSE]);
    let form = |form: &str, query: &str| {
        issue(
            &kernel,
            Verb::Source,
            &format!("urn:iki:store:graph-{form}"),
            &[("graph", &both()), ("query", query)],
            &cap,
        )
        .unwrap_or_else(|e| panic!("graph-{form}: {e:?}"))
    };
    assert!(form("select", JOIN).contains("joined"));
    assert!(form(
        "ask",
        "ASK { <urn:item:1> <urn:annotated-by> ?n . ?n <urn:body> \"joined\" }"
    )
    .contains("true"));
    assert!(form(
        "construct",
        "CONSTRUCT { ?item <urn:says> ?body } WHERE { ?item <urn:annotated-by> ?n . ?n <urn:body> ?body }"
    )
    .contains("<urn:item:1> <urn:says> \"joined\""));
    // DESCRIBE reads the dataset's default graph, which over a set is the merge of it —
    // so a subject in BROWSE describes through a set that includes BROWSE.
    assert!(form("describe", "DESCRIBE <urn:note:1>").contains("\"joined\""));
}

// ----------------------------------------------------------- order and the cache

#[test]
fn order_and_repetition_give_the_same_answer() {
    let kernel = owned();
    let cap = grants(&[LEDGER, BROWSE]);
    let expected = select(&kernel, &both(), JOIN, &cap).unwrap();
    for spelling in [
        format!("{BROWSE} {LEDGER}"),
        format!("{LEDGER} {BROWSE} {LEDGER}"),
        format!("\t{BROWSE}\n{LEDGER}\n"),
    ] {
        assert_eq!(
            select(&kernel, &spelling, JOIN, &cap).unwrap(),
            expected,
            "`{spelling}`"
        );
    }
}

/// A store shared with a writer the kernel cannot see, seeded through that writer.
fn shared(writes: SharerWrites) -> (Kernel, Arc<Store>) {
    let (store, handle) = DurableStore::in_memory_shared_declaring(writes).unwrap();
    handle
        .load_from_slice(RdfFormat::NQuads, corpus().as_bytes())
        .unwrap();
    (Kernel::new(Arc::new(space(store))), handle)
}

fn sharer_writes(handle: &Store, nquads: &str) {
    handle
        .load_from_slice(RdfFormat::NQuads, nquads.as_bytes())
        .unwrap();
}

/// ★ Order-independence OF THE CACHE, observed the only way a cache hit is observable: an
/// invisible write. The canonical spelling is read once (computed, cached); the sharer then
/// breaks its promise about BROWSE; a DIFFERENT spelling, never read before, still serves
/// the pre-write answer — so it was served from the canonical spelling's entry and was not
/// evaluated. The kernel write at the end is the other half: it cuts that entry for every
/// spelling.
#[test]
fn every_spelling_of_a_set_shares_one_cache_entry() {
    let (kernel, handle) = shared(SharerWrites::only_the_default_graph());
    let cap = Capability::scoped([
        cap_read_graph(LEDGER),
        cap_read_graph(BROWSE),
        cap_write_graph(BROWSE),
    ]);
    assert_eq!(
        select(&kernel, &both(), JOIN, &cap).unwrap(),
        ["urn:item:1,joined"]
    );

    sharer_writes(
        &handle,
        &format!(
            "<urn:item:1> <urn:annotated-by> <urn:note:2> <{LEDGER}> .\n\
             <urn:note:2> <urn:body> \"unseen\" <{BROWSE}> .\n"
        ),
    );
    for spelling in [
        format!("{BROWSE} {LEDGER}"),
        format!("{LEDGER}\n{BROWSE}\n"),
    ] {
        assert_eq!(
            select(&kernel, &spelling, JOIN, &cap).unwrap(),
            ["urn:item:1,joined"],
            "`{spelling:?}` was evaluated rather than served from the canonical entry"
        );
    }
    // The control that the invisible write really landed: the broad door is never cached
    // on a shared store, and it sees the new note.
    let broad = issue(
        &kernel,
        Verb::Source,
        "urn:iki:store:select",
        &[("query", "ASK { GRAPH ?g { ?n <urn:body> \"unseen\" } }")],
        &Capability::scoped([CAP_READ.to_string()]),
    )
    .unwrap();
    assert!(broad.contains("true"), "{broad}");

    // A write through the kernel cuts the canonical entry, and every spelling recomputes.
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", BROWSE),
            (
                "content",
                &format!("INSERT DATA {{ GRAPH <{BROWSE}> {{ <urn:x> <urn:y> <urn:z> }} }}"),
            ),
        ],
        &cap,
    )
    .expect("the scoped write");
    for spelling in [both(), format!("{BROWSE} {LEDGER}")] {
        let rows = select(&kernel, &spelling, JOIN, &cap).unwrap();
        assert_eq!(rows.len(), 2, "`{spelling:?}` kept a cut entry: {rows:?}");
    }
}

/// ★ Coverage is all-or-nothing: with BROWSE writable by the sharer (gonk's shape), a read
/// of LEDGER alone is cached and a join including BROWSE is NOT — the join is never cached
/// while any member is uncovered. Each half observed by an invisible write to LEDGER.
#[test]
fn one_uncovered_graph_makes_the_whole_read_uncached() {
    let (kernel, handle) = shared(SharerWrites::only_the_default_graph().and_named_graph(BROWSE));
    let cap = grants(&[LEDGER, BROWSE]);
    let ledger_only = "SELECT ?note WHERE { ?item <urn:annotated-by> ?note }";

    assert_eq!(
        select(&kernel, LEDGER, ledger_only, &cap).unwrap(),
        ["urn:note:1"]
    );
    assert_eq!(
        select(&kernel, &both(), ledger_only, &cap).unwrap(),
        ["urn:note:1"]
    );

    // The sharer breaks its promise about LEDGER, which only the cached read can hide.
    sharer_writes(
        &handle,
        &format!("<urn:item:2> <urn:annotated-by> <urn:note:9> <{LEDGER}> .\n"),
    );
    assert_eq!(
        select(&kernel, LEDGER, ledger_only, &cap).unwrap(),
        ["urn:note:1"],
        "LEDGER alone is covered, so it was cached (and the broken promise is invisible)"
    );
    let mut rows = select(&kernel, &both(), ledger_only, &cap).unwrap();
    rows.sort();
    assert_eq!(
        rows,
        ["urn:note:1", "urn:note:9"],
        "a set including the uncovered BROWSE must be evaluated every time"
    );
}

// --------------------------------------------------------------- the declaration

/// Declared is still the family wildcard, and `graph` declares the wire's own class: it
/// is a whitespace-separated LIST, which `xsd:anyURI` would tell a validator is malformed.
#[test]
fn the_scoped_forms_declare_a_graph_list_under_the_family_wildcard() {
    let space = space(DurableStore::in_memory().unwrap());
    for form in ["select", "ask", "construct", "describe"] {
        let request = Request::new(
            Verb::Meta,
            Iri::parse(format!("urn:iki:store:graph-{form}")).unwrap(),
        );
        let ikigai_core::Resolution::Hit(hit) = space.resolve(&request, &Scope::empty()) else {
            panic!("graph-{form} is not bound");
        };
        let desc = hit.endpoint.describe();
        assert_eq!(desc.requires, [CAP_READ_GRAPH], "graph-{form}");
        let graph = desc.inputs.iter().find(|i| i.name == "graph").unwrap();
        assert!(graph.required, "graph-{form}");
        assert_eq!(
            graph.class.as_deref(),
            Some("http://www.w3.org/2001/XMLSchema#string"),
            "graph-{form}"
        );
        assert!(desc.summary.contains("every one of them"), "graph-{form}");
    }
    // #373: the broad forms say which dataset they read, and how to read all of it.
    let request = Request::new(Verb::Meta, Iri::parse("urn:iki:store:select").unwrap());
    let ikigai_core::Resolution::Hit(hit) = space.resolve(&request, &Scope::empty()) else {
        panic!("select is not bound");
    };
    let summary = hit.endpoint.describe().summary;
    assert!(summary.contains("DEFAULT graph only"), "{summary}");
    assert!(summary.contains("UNION"), "{summary}");
}
