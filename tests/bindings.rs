//! `bindings=` through the kernel: the value reaches the evaluator as a **term**.
//!
//! `tests/` in `src/sparql.rs` prove the term constructors; these prove the door. The
//! difference matters: a helper that escapes correctly and an endpoint that never calls
//! it look identical from the outside.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

/// A store holding one ordinary title and one that is a SPARQL injection attempt, so a
/// query can be asked to find *either* and the answer says which one it found.
const HOSTILE: &str = r#"" } ; DROP ALL ; INSERT DATA { <urn:x> <urn:y> ""#;

fn kernel() -> Kernel {
    let store = DurableStore::in_memory().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    let turtle = format!(
        "<urn:example:ordinary> <http://purl.org/dc/terms/title> {} .\n\
         <urn:example:hostile>  <http://purl.org/dc/terms/title> {} .\n",
        ikigai_store::sparql::literal("ordinary"),
        ikigai_store::sparql::literal(HOSTILE),
    );
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
                .with_arg("content", ArgRef::Inline(turtle.into_bytes())),
            &Capability::root(),
        ),
    )
    .expect("seeding — and note it seeds THROUGH the term constructors");
    kernel
}

fn select(kernel: &Kernel, query: &str, bindings: Option<&str>) -> Result<String, Error> {
    let mut request = Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("as", ArgRef::Inline(b"text/csv".to_vec()));
    if let Some(json) = bindings {
        request = request.with_arg("bindings", ArgRef::Inline(json.as_bytes().to_vec()));
    }
    block_on(kernel.issue(request, &Capability::root()))
        .map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
}

const BY_TITLE: &str = "SELECT * WHERE { ?s <http://purl.org/dc/terms/title> ?title }";

/// ★ The test the whole argument exists for. The hostile text is a *value*: it selects
/// the row whose title happens to be that text, the store still has both its statements,
/// and nothing was dropped.
#[test]
fn a_hostile_binding_is_matched_as_data_not_executed_as_syntax() {
    let kernel = kernel();
    // Through the re-exported term types, which is how a consumer reaches them without
    // taking its own `oxigraph` dependency and risking a second `Term`.
    use ikigai_store::sparql::{bindings_json, Literal, Term};
    let json = bindings_json([("title", Term::from(Literal::new_simple_literal(HOSTILE)))]);
    let out = select(&kernel, BY_TITLE, Some(&json)).expect("the bound query");
    assert!(out.contains("urn:example:hostile"), "{out}");
    assert!(!out.contains("urn:example:ordinary"), "{out}");

    // And the dataset is intact — `DROP ALL` inside the value did nothing.
    let all = select(&kernel, BY_TITLE, None).expect("an unbound query");
    assert_eq!(all.lines().count(), 3, "header + two rows: {all}");
}

/// The common case, written as itself: a bare JSON string is a plain literal, so a
/// consumer needs no term vocabulary for the 90% path.
#[test]
fn a_bare_json_string_binds_as_a_plain_literal() {
    let kernel = kernel();
    let out = select(&kernel, BY_TITLE, Some(r#"{"title": "ordinary"}"#)).expect("bound");
    assert!(out.contains("urn:example:ordinary"), "{out}");
    assert!(!out.contains("urn:example:hostile"), "{out}");
}

#[test]
fn an_iri_binds_in_the_subject_position() {
    let kernel = kernel();
    let out = select(
        &kernel,
        BY_TITLE,
        Some(r#"{"s": {"type":"uri","value":"urn:example:ordinary"}}"#),
    )
    .expect("bound");
    assert!(out.contains("urn:example:ordinary"), "{out}");
    assert!(!out.contains("urn:example:hostile"), "{out}");
}

/// ⚠ The constraint a consumer will hit first, and the reason the refusal is rewritten
/// rather than passed through: oxigraph's own sentence is true and says nothing about
/// the fix. **This also pins the upstream wording** this crate matches on — a rewording
/// upstream fails here rather than silently degrading the message.
#[test]
fn an_unprojected_binding_is_refused_with_the_fix() {
    let kernel = kernel();
    let err = select(
        &kernel,
        // ?title is used but not projected.
        "SELECT ?s WHERE { ?s <http://purl.org/dc/terms/title> ?title }",
        Some(r#"{"title": "ordinary"}"#),
    )
    .expect_err("an unprojected binding");
    let text = err.to_string();
    assert!(
        matches!(&err, Error::InvalidArgument { name, .. } if name == "bindings"),
        "{err:?}"
    );
    assert!(
        text.contains("projection"),
        "upstream wording changed: {text}"
    );
    assert!(
        text.contains("SELECT *"),
        "the fix is not in the message: {text}"
    );
    assert!(
        text.contains("?title"),
        "the message does not name the binding: {text}"
    );
}

/// ★ The half that makes the argument trustworthy: a binding the query does not mention
/// is an ERROR, not a silent no-op — otherwise a filter you thought was applied could
/// silently not be, which is the failure this argument exists to prevent arriving by the
/// back door. Enforced upstream; this pins that it stays enforced.
#[test]
fn a_binding_the_query_never_mentions_is_refused_not_ignored() {
    let kernel = kernel();
    let err = select(&kernel, BY_TITLE, Some(r#"{"nosuchvariable": "x"}"#))
        .expect_err("a binding for a variable the query does not have");
    assert!(
        matches!(&err, Error::InvalidArgument { name, .. } if name == "bindings"),
        "{err:?}"
    );
}

/// ASK, CONSTRUCT and DESCRIBE have no projection to widen, so they bind any variable in
/// the pattern — which is worth a test because it is the exception to the rule above.
#[test]
fn the_other_query_forms_bind_any_variable_in_the_pattern() {
    let kernel = kernel();
    let ask = |json: &str| {
        let request = Request::new(Verb::Source, Iri::parse("urn:iki:store:ask").unwrap())
            .with_arg(
                "query",
                ArgRef::Inline(b"ASK { ?s <http://purl.org/dc/terms/title> ?title }".to_vec()),
            )
            .with_arg("bindings", ArgRef::Inline(json.as_bytes().to_vec()));
        String::from_utf8_lossy(
            &block_on(kernel.issue(request, &Capability::root()))
                .expect("the bound ASK")
                .bytes,
        )
        .into_owned()
    };
    assert!(ask(r#"{"title": "ordinary"}"#).contains("true"));
    assert!(ask(r#"{"title": "no such title"}"#).contains("false"));
}

/// A malformed `bindings` is refused by name and never ignored — an argument that is
/// silently dropped is worse than one that is rejected, because the caller believes the
/// value was bound.
#[test]
fn a_malformed_bindings_argument_is_refused_by_name() {
    let kernel = kernel();
    for json in [r#"{"title": null}"#, "not json", "[1,2]"] {
        let err = select(&kernel, BY_TITLE, Some(json)).expect_err("{json}");
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "bindings"),
            "{json}: {err:?}"
        );
    }
}

/// ★ The write doors refuse `bindings` rather than accepting one that would do nothing.
/// The asymmetry is upstream's — oxigraph binds into a prepared query and has nothing
/// for a prepared update — and the refusal is where a consumer learns it, with the
/// supported alternative named.
#[test]
fn the_write_doors_refuse_bindings_and_say_what_to_use_instead() {
    let kernel = kernel();
    for (iri, extra) in [
        ("urn:iki:store:update", None),
        (
            "urn:iki:store:graph-update",
            Some(("graph", "urn:example:g")),
        ),
    ] {
        let mut request = Request::new(Verb::Sink, Iri::parse(iri).unwrap())
            .with_arg(
                "content",
                ArgRef::Inline(b"INSERT DATA { <urn:a> <urn:b> ?v }".to_vec()),
            )
            .with_arg("bindings", ArgRef::Inline(br#"{"v": "x"}"#.to_vec()));
        if let Some((name, value)) = extra {
            request = request.with_arg(name, ArgRef::Inline(value.as_bytes().to_vec()));
        }
        let err = block_on(kernel.issue(request, &Capability::root()))
            .expect_err("a `bindings` argument on a write");
        let text = err.to_string();
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "bindings"),
            "{iri}: {err:?}"
        );
        assert!(text.contains("ikigai_store::sparql"), "{iri}: {text}");
    }
}

/// Bindings are optional, so every query that worked before still works — the argument
/// is additive for the two live consumers.
#[test]
fn a_query_without_bindings_is_unchanged() {
    let kernel = kernel();
    let out = select(&kernel, BY_TITLE, None).expect("an unbound query");
    assert_eq!(out.lines().count(), 3, "{out}");
}
