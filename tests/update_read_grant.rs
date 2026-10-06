//! An update with a `WHERE` READS, so it needs the read grant (ledger #751).
//!
//! The 2026-10-05 audit reproduced two ways a caller holding ONLY
//! `urn:cap:store:write:graph:<G>` could read `G` through `urn:iki:store:graph-update`
//! without changing anything: copy `G`'s quads into an escaping pattern and read them back
//! out of the refusal, which quoted the first escaping quad (probe p7); and, with the quote
//! redacted, guess a value and watch whether the update is refused (probe p7b, a boolean
//! oracle). These tests are those probes, kept, beside the rule that closes them and the
//! cases that must keep working: an update with no `WHERE` stays write-only.
//!
//! ★ Every refusal here must be decided on the GRANT, before evaluation — so each oracle
//! test compares two calls that differ only in whether `G` holds the guessed value, and
//! asserts they are indistinguishable.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{cap_read_graph, cap_write_graph, space, DurableStore, CAP_READ, CAP_WRITE};
use std::sync::Arc;

const SCOPED: &str = "urn:iki:store:graph-update";
const BROAD: &str = "urn:iki:store:update";
const TA: &str = "urn:tenant:a";
const TB: &str = "urn:tenant:b";

fn kernel() -> Kernel {
    let kernel = Kernel::with_meta_renderer(
        Arc::new(space(DurableStore::in_memory().unwrap())),
        Arc::new(ikigai_vocab::TurtleRenderer),
    );
    run(
        &kernel,
        Verb::Sink,
        BROAD,
        &[(
            "content",
            &format!(
                "INSERT DATA {{ GRAPH <{TA}> {{ <urn:a:1> <urn:p> \"a-secret\" }} \
                 GRAPH <{TB}> {{ <urn:b:1> <urn:p> \"b-secret\" }} \
                 <urn:d:1> <urn:p> \"default-secret\" }}"
            ),
        )],
        &Capability::root(),
    )
    .expect("seeding");
    kernel
}

fn run(
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

fn scoped(kernel: &Kernel, update: &str, cap: &Capability) -> Result<String, Error> {
    run(
        kernel,
        Verb::Sink,
        SCOPED,
        &[("graph", TA), ("content", update)],
        cap,
    )
}

fn write_only() -> Capability {
    Capability::scoped([cap_write_graph(TA)])
}

fn read_write() -> Capability {
    Capability::scoped([cap_write_graph(TA), cap_read_graph(TA)])
}

/// Whether `ASK { GRAPH <graph> { <s> <urn:p> ?o } }` holds, under root.
fn holds(kernel: &Kernel, graph: &str, subject: &str) -> bool {
    run(
        kernel,
        Verb::Source,
        "urn:iki:store:ask",
        &[(
            "query",
            &format!("ASK {{ GRAPH <{graph}> {{ <{subject}> <urn:p> ?o }} }}"),
        )],
        &Capability::root(),
    )
    .unwrap()
    .contains("true")
}

// ------------------------------------------------------------- the audit's probes

/// Probe p7: the refusal quoted the escaping quad, and the quad was `G`'s data.
#[test]
fn a_write_only_grant_cannot_read_the_graph_through_the_refusal() {
    let k = kernel();
    // Control: the read door refuses this caller.
    assert!(run(
        &k,
        Verb::Source,
        "urn:iki:store:graph-select",
        &[("graph", TA), ("query", "SELECT * { ?s ?p ?o }")],
        &write_only()
    )
    .is_err());
    let exfil = format!(
        "INSERT {{ GRAPH <urn:nowhere> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }}"
    );
    let err = scoped(&k, &exfil, &write_only()).expect_err("refused");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(
        !err.to_string().contains("a-secret") && !err.to_string().contains("urn:a:1"),
        "a write-only caller read graph {TA}'s data out of the refusal: {err}"
    );
}

/// Probe p7b: refused-or-not was a function of `G`'s contents.
#[test]
fn a_write_only_grant_has_no_boolean_oracle_over_the_graph() {
    let k = kernel();
    let probe = |guess: &str| {
        scoped(
            &k,
            &format!(
                "INSERT {{ GRAPH <urn:nowhere> {{ <urn:x> <urn:y> <urn:z> }} }} \
                 WHERE {{ GRAPH <{TA}> {{ ?s ?p \"{guess}\" }} }}"
            ),
            &write_only(),
        )
        .map_err(|e| e.to_string())
    };
    assert_eq!(
        probe("a-secret"),
        probe("not-there"),
        "the outcome reveals whether graph {TA} holds the literal \"a-secret\""
    );
}

// --------------------------------------------------------------- the rule as built

/// Every shape with a `WHERE` — including the ones the parser rewrites into one — is
/// refused for a write-only caller, as a `Denied` naming the read grant, with nothing
/// applied. And each is accepted (or refused only on its effects) under the read grant.
#[test]
fn the_read_grant_rule_classifies_every_operation() {
    let reads = [
        format!("DELETE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }}"),
        format!(
            "INSERT {{ GRAPH <{TA}> {{ ?s <urn:q> ?o }} }} WHERE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }}"
        ),
        "DELETE WHERE { GRAPH ?g { ?s ?p ?o } }".to_string(),
        format!("WITH <{TA}> DELETE {{ ?s ?p ?o }} WHERE {{ ?s ?p ?o }}"),
        format!("INSERT {{ GRAPH <{TA}> {{ <urn:x> <urn:y> <urn:z> }} }} WHERE {{}}"),
        format!("COPY <{TA}> TO <{TB}>"),
        format!("MOVE <{TA}> TO <{TB}>"),
        format!("ADD <{TA}> TO <{TB}>"),
        // One reading operation anywhere in a `;` chain is enough.
        format!(
            "INSERT DATA {{ GRAPH <{TA}> {{ <urn:x> <urn:y> <urn:z> }} }} ; \
             DELETE WHERE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }}"
        ),
    ];
    for update in &reads {
        let k = kernel();
        let err = scoped(&k, update, &write_only()).expect_err(&format!(
            "`{update}` reads {TA} and was let through on a write grant"
        ));
        let text = err.to_string();
        assert!(matches!(err, Error::Denied(_)), "`{update}`: {err:?}");
        assert!(text.contains(&cap_read_graph(TA)), "`{update}`: {text}");
        assert!(text.contains("Nothing was evaluated"), "`{update}`: {text}");
        assert!(holds(&k, TA, "urn:a:1"), "`{update}` changed {TA}");
        assert!(holds(&k, TB, "urn:b:1"), "`{update}` changed {TB}");
        // Under the read grant the same update is no longer refused FOR THE GRANT: either
        // applied, or refused on its effects (COPY/MOVE/ADD escape to TB).
        if let Err(e) = scoped(&kernel(), update, &read_write()) {
            assert!(
                !e.to_string().contains("Nothing was evaluated"),
                "`{update}` is still refused for the read grant while holding it: {e}"
            );
        }
    }

    let write_only_shapes = [
        format!("INSERT DATA {{ GRAPH <{TA}> {{ <urn:x> <urn:y> <urn:z> }} }}"),
        format!("DELETE DATA {{ GRAPH <{TA}> {{ <urn:a:1> <urn:p> \"a-secret\" }} }}"),
        format!("CLEAR GRAPH <{TA}>"),
        format!("DROP GRAPH <{TA}>"),
        "CLEAR ALL".to_string(),
        "DROP ALL".to_string(),
        // Identity rewrites produce no operation at all, so read nothing.
        format!("COPY <{TA}> TO <{TA}>"),
    ];
    for update in &write_only_shapes {
        scoped(&kernel(), update, &write_only())
            .unwrap_or_else(|e| panic!("`{update}` reads nothing and must stay write-only: {e}"));
    }
}

/// The broad read grant reads every graph, so it satisfies the scoped door's read half
/// too — it adds nothing the caller could not already read.
#[test]
fn the_broad_read_grant_also_satisfies_the_read_half() {
    let k = kernel();
    let cap = Capability::scoped([cap_write_graph(TA), CAP_READ.to_string()]);
    scoped(
        &k,
        &format!("DELETE WHERE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }}"),
        &cap,
    )
    .expect("write grant plus broad read");
    assert!(!holds(&k, TA, "urn:a:1"));
}

/// The success line's counts are a read (`+0` after an `INSERT DATA` means the quad was
/// already there), so a write-only caller gets the same line either way.
#[test]
fn a_write_only_grant_learns_nothing_from_the_success_line() {
    let k = kernel();
    let existing = format!("INSERT DATA {{ GRAPH <{TA}> {{ <urn:a:1> <urn:p> \"a-secret\" }} }}");
    let fresh = format!("INSERT DATA {{ GRAPH <{TA}> {{ <urn:a:2> <urn:p> \"new\" }} }}");
    let absent = format!("DELETE DATA {{ GRAPH <{TA}> {{ <urn:a:9> <urn:p> \"nope\" }} }}");
    let a = scoped(&k, &existing, &write_only()).unwrap();
    let b = scoped(&k, &fresh, &write_only()).unwrap();
    let c = scoped(&k, &absent, &write_only()).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, c);
    assert_eq!(a, format!("updated <{TA}>\n"));
    // Under the read grant the counts are there, and true.
    let counted = scoped(
        &k,
        &format!("INSERT DATA {{ GRAPH <{TA}> {{ <urn:a:3> <urn:p> \"three\" }} }}"),
        &read_write(),
    )
    .unwrap();
    assert_eq!(counted, format!("updated <{TA}>: +1 -0 quads\n"));
}

/// A refusal on EFFECTS (something escaped) names the scoped graph and where the escape
/// went — never the quad, even the caller's own.
#[test]
fn an_escape_refusal_names_graphs_and_never_data() {
    let k = kernel();
    let err = scoped(
        &k,
        "INSERT DATA { <urn:stray:subject> <urn:p> \"stray-value\" }",
        &read_write(),
    )
    .expect_err("a write to the default graph");
    let text = err.to_string();
    assert!(
        text.contains("DEFAULT graph") && text.contains(TA),
        "{text}"
    );
    assert!(
        !text.contains("urn:stray:subject") && !text.contains("stray-value"),
        "{text}"
    );
}

// --------------------------------------------------------------------- broad door

/// The same rule one level up: `urn:iki:store:update` with a `WHERE` reads the whole
/// dataset, so it needs `urn:cap:store:read` beside `urn:cap:store:write`.
#[test]
fn the_broad_door_needs_the_broad_read_grant_for_a_where() {
    let k = kernel();
    let write = Capability::scoped([CAP_WRITE.to_string()]);
    let exfil = format!(
        "INSERT {{ GRAPH <urn:nowhere> {{ ?s ?p ?o }} }} WHERE {{ GRAPH <{TA}> {{ ?s ?p ?o }} }}"
    );
    let err = run(&k, Verb::Sink, BROAD, &[("content", &exfil)], &write).expect_err("refused");
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(err.to_string().contains(CAP_READ), "{err}");
    assert!(
        !holds(&k, "urn:nowhere", "urn:a:1"),
        "nothing was evaluated"
    );

    // Write-only is still enough for a write that reads nothing, and learns no counts.
    let insert = "INSERT DATA { <urn:d:1> <urn:p> \"default-secret\" }";
    assert_eq!(
        run(&k, Verb::Sink, BROAD, &[("content", insert)], &write).unwrap(),
        "updated\n"
    );
    // With the read grant, the WHERE runs and the counts come back.
    let both = Capability::scoped([CAP_WRITE.to_string(), CAP_READ.to_string()]);
    let out = run(&k, Verb::Sink, BROAD, &[("content", &exfil)], &both).unwrap();
    assert!(out.starts_with("updated: ") && out.contains("->"), "{out}");
    assert!(holds(&k, "urn:nowhere", "urn:a:1"));
}

/// `urn:iki:store:load` reads nothing, so it stays write-only — but its counts are a read.
#[test]
fn load_reports_counts_only_to_a_reader() {
    let k = kernel();
    let doc = "<urn:d:1> <urn:p> \"default-secret\" .";
    let write = Capability::scoped([CAP_WRITE.to_string()]);
    assert_eq!(
        run(
            &k,
            Verb::Sink,
            "urn:iki:store:load",
            &[("content", doc)],
            &write
        )
        .unwrap(),
        "loaded text/turtle\n"
    );
    let both = Capability::scoped([CAP_WRITE.to_string(), CAP_READ.to_string()]);
    let out = run(
        &k,
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", doc)],
        &both,
    )
    .unwrap();
    assert!(out.contains("->"), "{out}");
}
