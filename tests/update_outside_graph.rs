//! A scoped UPDATE that reads a graph OUTSIDE `graph=` is refused, not applied as a no-op
//! (ledger #1606), and so is one whose `WHERE` reads the default graph or whose `DELETE`
//! names another graph or the default graph (ledger #1612).
//!
//! # The defect this pins
//!
//! `urn:iki:store:graph-update` evaluates an update against a private copy of the one graph
//! `graph=` names, so a `WHERE` that reads another graph can never see it: the boundary was
//! already exact. But the update then matched nothing and answered `updated <north>: +0 -0`,
//! which is what an update that had nothing to do says too. A caller who mistyped an IRI in
//! `WITH`, or thought `USING <south>` would let it copy from a sibling graph, got silence that
//! looked like success. That is the write-side twin of the read-side trap closed in 0.2.22
//! (ledger #388, `tests/graph_outside_set.rs`), and it is refused the same way: by name, before
//! anything is evaluated.
//!
//! Ledger #1612 closed the three no-ops left over: a `DELETE` naming another graph
//! (`DELETE { GRAPH <south> … }`, `DELETE DATA { GRAPH <south> … }`, or `WITH <south>` beside
//! `USING <north>`, which leaves `WITH` in the templates alone) removed nothing and answered
//! `-0`; and a pattern outside any `GRAPH` block read the private copy's empty default graph
//! and answered `+0 -0` (`COPY DEFAULT TO <north>` emptied north).
//!
//! # ★ Still a pair
//!
//! Every refused shape sits beside the in-scope spelling that must still apply, and beside the
//! shape that names no constant graph (`GRAPH ?g`), which stays confined to the private copy
//! rather than refused.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore};
use std::sync::Arc;

const NORTH: &str = "urn:example:north";
const SOUTH: &str = "urn:example:south";
const EAST: &str = "urn:example:east";

fn kernel() -> Kernel {
    let store = DurableStore::in_memory().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    let seed = format!(
        "INSERT DATA {{ \
           GRAPH <{NORTH}> {{ <urn:example:north:1> <urn:p> \"north\" }} \
           GRAPH <{SOUTH}> {{ <urn:example:south:1> <urn:p> \"south\" }} \
           GRAPH <{EAST}> {{ <urn:example:east:1> <urn:p> \"east\" }} \
           <urn:example:default:1> <urn:p> \"default\" \
         }}"
    );
    issue(
        &kernel,
        "urn:iki:store:update",
        Verb::Sink,
        &[("content", &seed)],
        &Capability::root(),
    )
    .expect("seeding");
    kernel
}

fn issue(
    kernel: &Kernel,
    iri: &str,
    verb: Verb,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, cap)).map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

/// Read AND write grants on every graph in the store, so a refusal here is about what the door
/// evaluates against and never about authority.
fn every_grant() -> Capability {
    Capability::scoped([
        cap_write_graph(NORTH),
        cap_read_graph(NORTH),
        cap_write_graph(SOUTH),
        cap_read_graph(SOUTH),
        cap_write_graph(EAST),
        cap_read_graph(EAST),
    ])
}

fn scoped(kernel: &Kernel, update: &str) -> Result<String, Error> {
    issue(
        kernel,
        "urn:iki:store:graph-update",
        Verb::Sink,
        &[("graph", NORTH), ("content", update)],
        &every_grant(),
    )
}

/// The subjects of `graph`, sorted, read under root.
fn subjects(kernel: &Kernel, graph: &str) -> Vec<String> {
    let answer = issue(
        kernel,
        "urn:iki:store:select",
        Verb::Source,
        &[
            (
                "query",
                &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}"),
            ),
            ("as", "text/csv"),
        ],
        &Capability::root(),
    )
    .expect("reading");
    let mut rows: Vec<String> = answer.lines().skip(1).map(str::to_string).collect();
    rows.sort();
    rows
}

/// The refusal, checked for its shape: `InvalidArgument` on `content`, naming each outside
/// graph, saying `graph=` is the dataset, and that nothing was applied.
fn assert_refused_naming(result: Result<String, Error>, outside: &[&str], case: &str) {
    let named: Vec<String> = outside.iter().map(|iri| format!("<{iri}>")).collect();
    let named: Vec<&str> = named.iter().map(String::as_str).collect();
    assert_refused_saying(result, &named, case);
}

/// [`assert_refused_naming`], for refusals that name something other than an IRI.
fn assert_refused_saying(result: Result<String, Error>, says: &[&str], case: &str) {
    let err = match result {
        Ok(answer) => panic!("{case}: applied instead of refused: {answer:?}"),
        Err(err) => err,
    };
    let Error::InvalidArgument { name, detail } = &err else {
        panic!("{case}: not InvalidArgument: {err:?}");
    };
    assert_eq!(name, "content", "{case}");
    for needle in says {
        assert!(
            detail.contains(needle),
            "{case}: the refusal does not say {needle}: {detail}"
        );
    }
    assert!(
        detail.contains("graph="),
        "{case}: the refusal does not say `graph=` is the dataset: {detail}"
    );
    assert!(
        detail.contains("Nothing was applied"),
        "{case}: the refusal does not say nothing was applied: {detail}"
    );
}

/// ★ The item's own case (`WITH <south>` whose `WHERE` reads only south) and one per place an
/// update can name the graph it reads: each names `SOUTH` and nothing in `NORTH` changes.
#[test]
fn a_constant_graph_outside_the_scope_is_refused_wherever_the_update_reads_it() {
    for (case, update) in [
        (
            "WITH <south>, the item's own case",
            format!(
                "WITH <{SOUTH}> INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
        ),
        (
            "WITH <south>, deleting",
            format!("WITH <{SOUTH}> DELETE {{ ?s <urn:p> ?o }} WHERE {{ ?s <urn:p> ?o }}"),
        ),
        (
            "USING <south>",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} USING <{SOUTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
        ),
        (
            "USING NAMED <south>, read through a variable graph",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} USING NAMED <{SOUTH}> \
                 WHERE {{ GRAPH ?g {{ ?s <urn:p> ?o }} }}"
            ),
        ),
        (
            "USING NAMED <south> beside USING NAMED <north>",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 USING NAMED <{NORTH}> USING NAMED <{SOUTH}> \
                 WHERE {{ GRAPH ?g {{ ?s <urn:p> ?o }} }}"
            ),
        ),
        (
            "INSERT … WHERE { GRAPH <south> … }",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }}"
            ),
        ),
        (
            "DELETE WHERE { GRAPH <south> … }",
            format!("DELETE WHERE {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }}"),
        ),
        (
            "GRAPH <south> beside GRAPH <north>",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?v }} \
                 OPTIONAL {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }} }}"
            ),
        ),
        (
            "FILTER NOT EXISTS { GRAPH <south> … }",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:fresh> true }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?v }} \
                 FILTER NOT EXISTS {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }} }}"
            ),
        ),
        (
            "a sub-select reading south",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ <urn:example:n> <urn:count> ?c }} }} \
                 WHERE {{ SELECT (COUNT(*) AS ?c) WHERE {{ GRAPH <{SOUTH}> {{ ?s ?p ?o }} }} }}"
            ),
        ),
        (
            "ADD <south> TO <north>, which reads south",
            format!("ADD <{SOUTH}> TO <{NORTH}>"),
        ),
        (
            "COPY <south> TO <north>, which would have emptied north",
            format!("COPY <{SOUTH}> TO <{NORTH}>"),
        ),
        (
            "MOVE <south> TO <north>, which reads south and drops it",
            format!("MOVE <{SOUTH}> TO <{NORTH}>"),
        ),
        (
            "a second operation in a `;` chain",
            format!(
                "INSERT DATA {{ GRAPH <{NORTH}> {{ <urn:example:north:2> <urn:p> \"ok\" }} }} ; \
                 INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }}"
            ),
        ),
    ] {
        let kernel = kernel();
        let before = (subjects(&kernel, NORTH), subjects(&kernel, SOUTH));
        assert_refused_naming(scoped(&kernel, &update), &[SOUTH], case);
        assert_eq!(
            (subjects(&kernel, NORTH), subjects(&kernel, SOUTH)),
            before,
            "{case}: a refused update changed the store"
        );
    }
}

/// Every outside graph is named, once each, and the scoped graph is never named as outside.
#[test]
fn every_outside_graph_is_named_once() {
    let kernel = kernel();
    let update = format!(
        "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
         USING NAMED <{NORTH}> USING NAMED <{EAST}> USING NAMED <{SOUTH}> \
         WHERE {{ GRAPH <{NORTH}> {{ ?s ?p ?v }} GRAPH <{SOUTH}> {{ ?s ?p ?o }} \
         GRAPH <{EAST}> {{ ?s ?p ?o }} GRAPH <{SOUTH}> {{ ?s ?q ?o }} }}"
    );
    let result = scoped(&kernel, &update);
    let detail = match &result {
        Err(Error::InvalidArgument { detail, .. }) => detail.clone(),
        other => panic!("not refused: {other:?}"),
    };
    assert_refused_naming(result, &[SOUTH, EAST], "two outside");
    for iri in [SOUTH, EAST] {
        assert_eq!(
            detail.matches(&format!("<{iri}>")).count(),
            1,
            "<{iri}> named more than once: {detail}"
        );
    }
    // `<north>` appears as the scope (`graph=…`), never in the list of outside graphs, which
    // comes first.
    let list = detail.split(", which ").next().unwrap_or_default();
    assert!(
        !list.contains(NORTH),
        "the scope named as outside: {detail}"
    );
}

/// ★ The other half of the pair: naming the scoped graph itself in every one of those places
/// still applies, and a `GRAPH ?g` names no constant graph, so it stays confined to the
/// private copy rather than refused. (A bare pattern applied here as `+0 -0` until ledger
/// #1612; it is refused now, below.)
#[test]
fn the_scoped_graph_and_variable_graphs_still_apply() {
    for (case, update, added) in [
        (
            "WITH <north>",
            format!("WITH <{NORTH}> INSERT {{ ?s <urn:copied> ?o }} WHERE {{ ?s <urn:p> ?o }}"),
            1,
        ),
        (
            "USING <north>",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} USING <{NORTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
            1,
        ),
        (
            "USING NAMED <north>",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} USING NAMED <{NORTH}> \
                 WHERE {{ GRAPH ?g {{ ?s <urn:p> ?o }} }}"
            ),
            1,
        ),
        (
            "GRAPH <north> in the WHERE",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o }} }}"
            ),
            1,
        ),
        (
            "GRAPH ?g, which can bind north and nothing else",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:from> ?g }} }} \
                 WHERE {{ GRAPH ?g {{ ?s <urn:p> ?o }} }}"
            ),
            1,
        ),
    ] {
        let kernel = kernel();
        let answer = scoped(&kernel, &update)
            .unwrap_or_else(|e| panic!("{case}: expected it to apply, got {e:?}"));
        assert!(answer.contains(&format!("+{added} -0")), "{case}: {answer}");
        assert_eq!(
            subjects(&kernel, SOUTH),
            vec!["urn:example:south:1,urn:p,south".to_string()],
            "{case} changed another graph"
        );
        // A variable graph binds only the scoped graph: nothing of south reached north.
        assert!(
            !subjects(&kernel, NORTH)
                .iter()
                .any(|row| row.contains("south")),
            "{case}: south's data reached north"
        );
    }
}

/// Every graph but `NORTH`, and the default graph, as they were seeded: what a refused update
/// must leave alone, beside `NORTH` itself.
fn the_store(kernel: &Kernel) -> Vec<Vec<String>> {
    let mut graphs: Vec<Vec<String>> = [NORTH, SOUTH, EAST]
        .iter()
        .map(|graph| subjects(kernel, graph))
        .collect();
    let answer = issue(
        kernel,
        "urn:iki:store:select",
        Verb::Source,
        &[
            ("query", "SELECT ?s ?p ?o WHERE { ?s ?p ?o }"),
            ("as", "text/csv"),
        ],
        &Capability::root(),
    )
    .expect("reading the default graph");
    graphs.push(answer.lines().skip(1).map(str::to_string).collect());
    graphs
}

/// ★ Ledger #1612 (1): a `DELETE` that names another graph, or the default graph, removes
/// nothing from the private copy and answered `-0`. Each is refused by name, before
/// evaluation, and the store is unchanged.
#[test]
fn a_delete_naming_another_graph_or_the_default_graph_is_refused() {
    for (case, update, says) in [
        (
            "DELETE { GRAPH <south> } WHERE { GRAPH <north> }",
            format!(
                "DELETE {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o }} }}"
            ),
            format!("<{SOUTH}>"),
        ),
        (
            "DELETE DATA { GRAPH <south> }",
            format!(
                "DELETE DATA {{ GRAPH <{SOUTH}> {{ <urn:example:south:1> <urn:p> \"south\" }} }}"
            ),
            format!("<{SOUTH}>"),
        ),
        (
            "DELETE DATA in the default graph",
            "DELETE DATA { <urn:example:default:1> <urn:p> \"default\" }".to_string(),
            "the default graph".to_string(),
        ),
        (
            "a DELETE template in the default graph, beside a GRAPH <north> WHERE",
            format!("DELETE {{ ?s <urn:p> ?o }} WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o }} }}"),
            "the default graph".to_string(),
        ),
        (
            "WITH <south> beside USING <north>, deleting: WITH survives in the template alone",
            format!(
                "WITH <{SOUTH}> DELETE {{ ?s <urn:p> ?o }} USING <{NORTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
            format!("<{SOUTH}>"),
        ),
        (
            "WITH <south> beside USING <north>, deleting and inserting",
            format!(
                "WITH <{SOUTH}> DELETE {{ ?s <urn:p> ?o }} \
                 INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:moved> ?o }} }} USING <{NORTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
            format!("<{SOUTH}>"),
        ),
        (
            "the second operation in a `;` chain",
            format!(
                "INSERT DATA {{ GRAPH <{NORTH}> {{ <urn:example:north:2> <urn:p> \"ok\" }} }} ; \
                 DELETE DATA {{ GRAPH <{SOUTH}> {{ <urn:example:south:1> <urn:p> \"south\" }} }}"
            ),
            format!("<{SOUTH}>"),
        ),
    ] {
        let kernel = kernel();
        let before = the_store(&kernel);
        assert_refused_saying(scoped(&kernel, &update), &[&says, "DELETE"], case);
        assert_eq!(
            the_store(&kernel),
            before,
            "{case}: a refused update changed the store"
        );
    }
}

/// `DELETE DATA` has no `WHERE`, so a caller holding the write grant alone reaches the refusal
/// too: it is decided on the text, and says nothing about what any graph holds.
#[test]
fn a_write_only_callers_delete_naming_another_graph_is_refused_on_the_text() {
    let kernel = kernel();
    let before = the_store(&kernel);
    let result = issue(
        &kernel,
        "urn:iki:store:graph-update",
        Verb::Sink,
        &[
            ("graph", NORTH),
            (
                "content",
                &format!("DELETE DATA {{ GRAPH <{SOUTH}> {{ <urn:example:nothing> <urn:p> 1 }} }}"),
            ),
        ],
        &Capability::scoped([cap_write_graph(NORTH)]),
    );
    assert_refused_naming(result, &[SOUTH], "write-only DELETE DATA");
    assert_eq!(the_store(&kernel), before);
}

/// ★ Ledger #1612 (3): a pattern outside any `GRAPH` block, with no `WITH` or `USING` to name
/// its graph, reads the private copy's empty default graph. Refused, saying how to name the
/// scoped graph instead; `COPY DEFAULT TO <north>` emptied north.
#[test]
fn a_pattern_that_reads_the_default_graph_is_refused() {
    for (case, update) in [
        (
            "the item's case, INSERT { GRAPH <north> } WHERE { ?s ?p ?o }",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} WHERE {{ ?s <urn:p> ?o }}"
            ),
        ),
        (
            "DELETE WHERE with a bare pattern",
            "DELETE WHERE { ?s <urn:p> ?o }".to_string(),
        ),
        (
            "COPY DEFAULT TO <north>, which would have emptied north",
            format!("COPY DEFAULT TO <{NORTH}>"),
        ),
        (
            "ADD DEFAULT TO <north>",
            format!("ADD DEFAULT TO <{NORTH}>"),
        ),
        (
            "USING NAMED <north> alone, which leaves the default graph empty",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} USING NAMED <{NORTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
        ),
        (
            "a bare pattern beside a GRAPH <north> block",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o }} ?s <urn:q> ?x }}"
            ),
        ),
        (
            "FILTER NOT EXISTS over the default graph, which held for every row",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:fresh> true }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o }} \
                 FILTER NOT EXISTS {{ ?s <urn:p> ?o }} }}"
            ),
        ),
        (
            "a property path outside any GRAPH block",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} WHERE {{ ?s <urn:p>+ ?o }}"
            ),
        ),
        (
            "a sub-select over the default graph",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ <urn:example:n> <urn:count> ?c }} }} \
                 WHERE {{ SELECT (COUNT(*) AS ?c) WHERE {{ ?s ?p ?o }} }}"
            ),
        ),
    ] {
        let kernel = kernel();
        let before = the_store(&kernel);
        assert_refused_saying(
            scoped(&kernel, &update),
            &[
                "default graph",
                &format!("GRAPH <{NORTH}>"),
                &format!("WITH <{NORTH}>"),
            ],
            case,
        );
        assert_eq!(
            the_store(&kernel),
            before,
            "{case}: a refused update changed the store"
        );
    }
}

/// The other half of the ledger #1612 pair: a `DELETE` of the scoped graph, by `GRAPH`, `WITH`
/// or a variable graph, and a `WHERE` that reads nothing outside a `GRAPH` block, or whose
/// default graph `WITH`/`USING` makes the scoped graph, still apply and say what they did.
#[test]
fn deletes_and_patterns_in_the_scoped_graph_still_apply() {
    for (case, update, counts) in [
        (
            "DELETE DATA { GRAPH <north> }",
            format!(
                "DELETE DATA {{ GRAPH <{NORTH}> {{ <urn:example:north:1> <urn:p> \"north\" }} }}"
            ),
            "+0 -1",
        ),
        (
            "WITH <north> DELETE with a bare pattern",
            format!("WITH <{NORTH}> DELETE {{ ?s <urn:p> ?o }} WHERE {{ ?s <urn:p> ?o }}"),
            "+0 -1",
        ),
        (
            "WITH <north> beside USING <north>",
            format!(
                "WITH <{NORTH}> DELETE {{ ?s <urn:p> ?o }} USING <{NORTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
            "+0 -1",
        ),
        (
            "USING <north> with a bare pattern",
            format!(
                "DELETE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o }} }} USING <{NORTH}> \
                 WHERE {{ ?s <urn:p> ?o }}"
            ),
            "+0 -1",
        ),
        (
            "DELETE { GRAPH ?g }, which can bind north alone",
            "DELETE WHERE { GRAPH ?g { ?s <urn:p> ?o } }".to_string(),
            "+0 -1",
        ),
        (
            "an empty WHERE reads no graph",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ <urn:example:north:2> <urn:p> 1 }} }} WHERE {{ }}"
            ),
            "+1 -0",
        ),
        (
            "a VALUES block alone reads no graph",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:p> 1 }} }} \
                 WHERE {{ VALUES ?s {{ <urn:example:north:2> }} }}"
            ),
            "+1 -0",
        ),
        (
            "FILTER NOT EXISTS inside the GRAPH <north> block",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:fresh> true }} }} \
                 WHERE {{ GRAPH <{NORTH}> {{ ?s <urn:p> ?o \
                 FILTER NOT EXISTS {{ ?s <urn:fresh> true }} }} }}"
            ),
            "+1 -0",
        ),
    ] {
        let seeded = the_store(&kernel());
        let kernel = kernel();
        let answer = scoped(&kernel, &update)
            .unwrap_or_else(|e| panic!("{case}: expected it to apply, got {e:?}"));
        assert!(answer.contains(counts), "{case}: {answer}");
        assert_eq!(
            the_store(&kernel)[1..],
            seeded[1..],
            "{case} changed a graph outside north"
        );
    }
}

/// ★ Ordering against the read rule (ledger #751): a caller holding the write grant alone is
/// refused on the GRANT first, as before, whatever graph its `WHERE` names, so this refusal
/// tells a write-only caller nothing the read-grant refusal did not.
#[test]
fn a_write_only_caller_is_refused_on_the_read_grant_first() {
    let kernel = kernel();
    let update = format!(
        "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
         WHERE {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }}"
    );
    let err = issue(
        &kernel,
        "urn:iki:store:graph-update",
        Verb::Sink,
        &[("graph", NORTH), ("content", &update)],
        &Capability::scoped([cap_write_graph(NORTH)]),
    )
    .expect_err("a WHERE without the read grant");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
}

/// The broad door reads the whole dataset, so the same update there is not refused: this
/// refusal is about `graph-update`'s private copy, not about the update.
#[test]
fn the_broad_door_still_reads_another_graph() {
    let kernel = kernel();
    let answer = issue(
        &kernel,
        "urn:iki:store:update",
        Verb::Sink,
        &[(
            "content",
            &format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} \
                 WHERE {{ GRAPH <{SOUTH}> {{ ?s <urn:p> ?o }} }}"
            ),
        )],
        &Capability::root(),
    )
    .expect("the broad door reads south");
    assert!(!answer.is_empty());
    assert!(subjects(&kernel, NORTH)
        .iter()
        .any(|row| row.starts_with("urn:example:south:1,urn:copied")));
}
