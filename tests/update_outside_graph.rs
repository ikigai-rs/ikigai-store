//! A scoped UPDATE that reads a graph OUTSIDE `graph=` is refused, not applied as a no-op
//! (ledger #1606).
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
//! # ★ Still a pair
//!
//! Every refused shape sits beside the in-scope spelling that must still apply, and beside the
//! shapes that name no constant graph (`GRAPH ?g`, a bare pattern), which stay confined to the
//! private copy rather than refused.

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
    let err = match result {
        Ok(answer) => panic!("{case}: applied instead of refused: {answer:?}"),
        Err(err) => err,
    };
    let Error::InvalidArgument { name, detail } = &err else {
        panic!("{case}: not InvalidArgument: {err:?}");
    };
    assert_eq!(name, "content", "{case}");
    for iri in outside {
        assert!(
            detail.contains(&format!("<{iri}>")),
            "{case}: the refusal does not name <{iri}>: {detail}"
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
/// still applies, and a `GRAPH ?g` or a bare pattern names no constant graph, so it stays
/// confined to the private copy rather than refused.
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
        (
            "a bare pattern, which reads the private copy's empty default graph",
            format!(
                "INSERT {{ GRAPH <{NORTH}> {{ ?s <urn:copied> ?o }} }} WHERE {{ ?s <urn:p> ?o }}"
            ),
            0,
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
