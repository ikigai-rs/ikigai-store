//! A binding must preserve the RDF term's identity, including a literal's base direction.
use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::sync::Arc;

fn select(k: &Kernel, bindings: &str) -> ikigai_core::Result<ikigai_core::Representation> {
    block_on(
        k.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(b"SELECT ?value WHERE {}".to_vec()))
                .with_arg("bindings", ArgRef::Inline(bindings.as_bytes().to_vec())),
            &Capability::root(),
        ),
    )
}

#[test]
fn an_invalid_direction_is_refused_instead_of_silently_discarded() {
    let k = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    for json in [
        r#"{"value":{"type":"literal","value":"text","xml:lang":"en","its:dir":"up"}}"#,
        r#"{"value":{"type":"literal","value":"text","its:dir":"rtl"}}"#,
        r#"{"value":{"type":"literal","value":"text","xml:lang":"en","its:dir":true}}"#,
    ] {
        let error = select(&k, json).expect_err("an invalid direction must be refused");
        let ikigai_core::Error::InvalidArgument { name, detail } = error else {
            panic!("expected InvalidArgument, got {error:?}");
        };
        assert_eq!(name, "bindings");
        assert!(detail.contains("`value`"), "{detail}");
        assert!(
            detail.contains("use a directional language-tagged literal"),
            "{detail}"
        );
    }
}

#[cfg(feature = "rdf-12")]
#[test]
fn directional_literals_round_trip_through_bindings_and_query_answers() {
    use ikigai_store::sparql::{bindings_json, Literal, Term};
    use oxigraph::model::BaseDirection;
    let k = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    for (direction, name) in [(BaseDirection::Ltr, "ltr"), (BaseDirection::Rtl, "rtl")] {
        let literal =
            Literal::new_directional_language_tagged_literal("quote\"\nمرحبا", "ar", direction)
                .unwrap();
        let json = bindings_json([("value", Term::from(literal))]);
        let encoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(encoded["value"]["its:dir"], name, "{json}");
        let result = select(&k, &json).unwrap();
        let answer: serde_json::Value = serde_json::from_slice(&result.bytes).unwrap();
        assert_eq!(answer["results"]["bindings"][0]["value"], encoded["value"]);
    }
}

#[cfg(not(feature = "rdf-12"))]
#[test]
fn a_direction_is_either_supported_by_unification_or_refused_without_changing_the_term() {
    let k = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    // A consumer may enable RDF 1.2 on oxigraph without enabling this crate's own feature.
    if let Ok(answer) = select(
        &k,
        r#"{"value":{"type":"literal","value":"text","xml:lang":"en","its:dir":"rtl"}}"#,
    ) {
        let answer: serde_json::Value = serde_json::from_slice(&answer.bytes).unwrap();
        assert_eq!(answer["results"]["bindings"][0]["value"]["its:dir"], "rtl");
    }
}

#[cfg(feature = "rdf-12")]
#[test]
fn a_directional_binding_matches_only_the_directional_term() {
    let k = Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())));
    block_on(k.issue(
        Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
            "content",
            ArgRef::Inline(br#"INSERT DATA { <urn:plain> <urn:p> "text"@en . <urn:directed> <urn:p> "text"@en--rtl }"#.to_vec()),
        ),
        &Capability::root(),
    )).unwrap();
    for (door, graph) in [
        ("urn:iki:store:select", None),
        ("urn:iki:store:graph-select", Some("urn:g")),
    ] {
        if graph.is_some() {
            block_on(k.issue(
                Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
                    "content",
                    ArgRef::Inline(b"COPY DEFAULT TO GRAPH <urn:g>".to_vec()),
                ),
                &Capability::root(),
            ))
            .unwrap();
        }
        let mut request = Request::new(Verb::Source, Iri::parse(door).unwrap())
            .with_arg("query", ArgRef::Inline(b"SELECT ?s ?value WHERE { ?s <urn:p> ?value }".to_vec()))
            .with_arg("bindings", ArgRef::Inline(br#"{"value":{"type":"literal","value":"text","xml:lang":"en","its:dir":"rtl"}}"#.to_vec()));
        if let Some(graph) = graph {
            request = request.with_arg("graph", ArgRef::Inline(graph.as_bytes().to_vec()));
        }
        let answer = block_on(k.issue(request, &Capability::root())).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&answer.bytes).unwrap();
        let rows = json["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["s"]["value"], "urn:directed");
    }
}
