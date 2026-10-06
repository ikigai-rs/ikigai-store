//! A present-but-unreadable argument is REFUSED, never treated as absent (ledger #751).
//!
//! `Invocation::inline_str` errors both for an absent argument and for one that is present
//! but not inline UTF-8, so reading an optional argument with `.ok()` merged the two. The
//! 2026-10-05 audit reproduced three consequences, each the opposite of what the caller
//! asked for and each silent: a `bindings` filter dropped so the query returned every row
//! (probe p1), a `load` into `graph=G` that landed in the DEFAULT graph (p2), and an
//! unreadable `as` replaced by the default serialization (p2b). These are those probes,
//! plus `load`'s `format`, which had the same shape.
//!
//! Every argument is tried in all three unreadable forms: by reference, by content id, and
//! inline bytes that are not UTF-8.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, ContentId, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

fn kernel() -> Kernel {
    let kernel = Kernel::with_meta_renderer(
        Arc::new(space(DurableStore::in_memory().unwrap())),
        Arc::new(ikigai_vocab::TurtleRenderer),
    );
    run(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[(
            "content",
            inline("INSERT DATA { <urn:p:alice> <urn:name> \"alice\" . <urn:p:bob> <urn:name> \"bob\" }"),
        )],
    )
    .expect("seeding");
    kernel
}

fn inline(s: &str) -> ArgRef {
    ArgRef::Inline(s.as_bytes().to_vec())
}

/// The three ways an argument can be present and still not be an inline UTF-8 value.
fn unreadable(value: &str) -> [(&'static str, ArgRef); 3] {
    [
        (
            "by content id",
            ArgRef::Content(ContentId::of(value.as_bytes())),
        ),
        (
            "by reference",
            ArgRef::Reference(Iri::parse("urn:example:elsewhere").unwrap()),
        ),
        ("not UTF-8", ArgRef::Inline(vec![0xff, 0xfe, 0x00])),
    ]
}

fn run(kernel: &Kernel, verb: Verb, iri: &str, args: &[(&str, ArgRef)]) -> Result<String, Error> {
    let mut request = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        request = request.with_arg(*name, value.clone());
    }
    block_on(kernel.issue(request, &Capability::root()))
        .map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

fn refused_by_name(got: Result<String, Error>, name: &str, label: &str) {
    match got {
        Err(Error::InvalidArgument { name: n, .. }) if n == name => {}
        other => panic!("`{name}` {label}: expected InvalidArgument naming it, got {other:?}"),
    }
}

/// Probe p1, read side: an unreadable `bindings` must not run the query unfiltered.
#[test]
fn an_unreadable_bindings_argument_is_refused_not_dropped() {
    let k = kernel();
    for iri in ["urn:iki:store:select", "urn:iki:store:graph-select"] {
        for (label, arg) in unreadable(r#"{"n":"alice"}"#) {
            let mut args = vec![
                ("query", inline("SELECT * WHERE { ?s <urn:name> ?n }")),
                ("bindings", arg),
            ];
            if iri.contains("graph-") {
                args.push(("graph", inline("urn:example:g")));
            }
            let got = run(&k, Verb::Source, iri, &args);
            if let Ok(rows) = &got {
                assert!(
                    !rows.contains("urn:p:bob"),
                    "{iri} {label}: ran unfiltered: {rows}"
                );
            }
            refused_by_name(got, "bindings", &format!("{iri} {label}"));
        }
    }
    // The control: an inline binding filters as it always did.
    let rows = run(
        &k,
        Verb::Source,
        "urn:iki:store:select",
        &[
            ("query", inline("SELECT * WHERE { ?s <urn:name> ?n }")),
            ("bindings", inline(r#"{"n":"alice"}"#)),
        ],
    )
    .unwrap();
    assert!(
        rows.contains("urn:p:alice") && !rows.contains("urn:p:bob"),
        "{rows}"
    );
}

/// Probe p1, write side: both Sinks refuse a `bindings` in ANY form, unreadable included.
#[test]
fn a_write_refuses_bindings_in_any_form() {
    let k = kernel();
    for iri in ["urn:iki:store:update", "urn:iki:store:graph-update"] {
        for (label, arg) in unreadable("{}") {
            let mut args = vec![
                (
                    "content",
                    inline("INSERT DATA { GRAPH <urn:example:g> { <urn:u> <urn:v> <urn:w> } }"),
                ),
                ("bindings", arg),
            ];
            if iri.contains("graph-") {
                args.push(("graph", inline("urn:example:g")));
            }
            refused_by_name(
                run(&k, Verb::Sink, iri, &args),
                "bindings",
                &format!("{iri} {label}"),
            );
        }
    }
}

/// Probe p2: an unreadable `graph` on `load` must not land the document in the default
/// graph.
#[test]
fn an_unreadable_load_graph_is_refused_not_loaded_into_the_default_graph() {
    for (label, arg) in unreadable("urn:g:target") {
        let k = kernel();
        let got = run(
            &k,
            Verb::Sink,
            "urn:iki:store:load",
            &[
                ("content", inline("<urn:s> <urn:p> <urn:o> .")),
                ("graph", arg),
            ],
        );
        refused_by_name(got, "graph", label);
        let anywhere = run(
            &k,
            Verb::Source,
            "urn:iki:store:ask",
            &[(
                "query",
                inline("ASK { { <urn:s> <urn:p> <urn:o> } UNION { GRAPH ?g { <urn:s> <urn:p> <urn:o> } } }"),
            )],
        )
        .unwrap();
        assert!(
            anywhere.contains("false"),
            "{label}: something was loaded: {anywhere}"
        );
    }
}

/// `load`'s `format`, same shape: an unreadable one is refused, not read as Turtle.
#[test]
fn an_unreadable_load_format_is_refused_not_defaulted() {
    for (label, arg) in unreadable("application/n-triples") {
        let got = run(
            &kernel(),
            Verb::Sink,
            "urn:iki:store:load",
            &[
                ("content", inline("<urn:s> <urn:p> <urn:o> .")),
                ("format", arg),
            ],
        );
        refused_by_name(got, "format", label);
    }
}

/// Probe p2b: an unreadable `as` must not be replaced by the default serialization —
/// on a result-set form and on a graph form.
#[test]
fn an_unreadable_as_is_refused_not_substituted() {
    let k = kernel();
    for (iri, query) in [
        ("urn:iki:store:select", "SELECT * WHERE { ?s ?p ?o }"),
        ("urn:iki:store:construct", "CONSTRUCT WHERE { ?s ?p ?o }"),
    ] {
        for (label, arg) in unreadable("text/csv") {
            let got = run(
                &k,
                Verb::Source,
                iri,
                &[("query", inline(query)), ("as", arg)],
            );
            refused_by_name(got, "as", &format!("{iri} {label}"));
        }
    }
    // Absent is still the default, which is the half that must not move.
    let json = run(
        &k,
        Verb::Source,
        "urn:iki:store:select",
        &[("query", inline("SELECT * WHERE { ?s ?p ?o }"))],
    )
    .unwrap();
    assert!(json.contains("\"results\""), "{json}");
}
