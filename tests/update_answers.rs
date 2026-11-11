//! Answer correctness through public doors, using scratch stores only.
use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore, SnapshotReader};
use std::collections::BTreeSet;
use std::sync::Arc;

fn issue(k: &Kernel, verb: Verb, door: &str, args: &[(&str, &str)]) -> ikigai_core::Result<String> {
    let request = args.iter().fold(
        Request::new(verb, Iri::parse(door).unwrap()),
        |r, (n, v)| r.with_arg(*n, ArgRef::Inline(v.as_bytes().to_vec())),
    );
    block_on(k.issue(request, &Capability::root())).map(|r| String::from_utf8(r.bytes).unwrap())
}
fn update(k: &Kernel, text: &str) -> ikigai_core::Result<String> {
    issue(k, Verb::Sink, "urn:iki:store:update", &[("content", text)])
}
fn scoped_update(k: &Kernel, text: &str) -> ikigai_core::Result<String> {
    issue(
        k,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[("graph", "urn:g"), ("content", text)],
    )
}
fn over(store: DurableStore) -> (Kernel, SnapshotReader) {
    let reader = store.reader();
    (Kernel::new(Arc::new(space(store))), reader)
}
fn kernel() -> Kernel {
    over(DurableStore::in_memory().unwrap()).0
}
fn ask(k: &Kernel, text: &str) -> bool {
    let answer = issue(k, Verb::Source, "urn:iki:store:ask", &[("query", text)]).unwrap();
    serde_json::from_str::<serde_json::Value>(&answer).unwrap()["boolean"]
        .as_bool()
        .unwrap()
}
fn graphs(k: &Kernel) -> BTreeSet<String> {
    issue(k, Verb::Source, "urn:iki:store:graphs", &[])
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}
fn quads(reader: &SnapshotReader) -> BTreeSet<String> {
    reader
        .snapshot_iter()
        .map(|q| q.unwrap().to_string())
        .collect()
}

// These are executable defect reproductions, not passing conformance claims. The parser
// lowers these operations before this crate sees them and omits destination registration.
// Run with: cargo test --test update_answers empty_source -- --ignored --nocapture
fn empty_source(operation: &str) {
    let mut missing = Vec::new();
    for source in ["GRAPH <urn:empty>", "DEFAULT"] {
        for destination_exists in [false, true] {
            let k = kernel();
            if source != "DEFAULT" {
                update(&k, "CREATE GRAPH <urn:empty>").unwrap();
            }
            if destination_exists {
                update(
                    &k,
                    "INSERT DATA { GRAPH <urn:destination> { <urn:s> <urn:p> 1 } }",
                )
                .unwrap();
            }
            update(
                &k,
                &format!("{operation} {source} TO GRAPH <urn:destination>"),
            )
            .unwrap();
            if !graphs(&k).contains("urn:destination") {
                missing.push(format!(
                    "source={source}, destination previously present={destination_exists}"
                ));
            } else {
                assert!(ask(&k, "ASK { GRAPH <urn:destination> {} }"));
            }
            assert_eq!(
                graphs(&k).contains("urn:empty"),
                source != "DEFAULT" && operation != "MOVE"
            );
        }
    }
    assert!(
        missing.is_empty(),
        "{operation} left no destination: {missing:?}"
    );
}

#[test]
#[ignore = "known defect: COPY of an empty source does not register its destination"]
fn copy_empty_source_registers_destination() {
    empty_source("COPY");
}
#[test]
#[ignore = "known defect: MOVE of an empty source does not register its destination"]
fn move_empty_source_registers_destination() {
    empty_source("MOVE");
}
#[test]
#[ignore = "known defect: ADD of an empty source does not register its destination"]
fn add_empty_source_registers_destination() {
    empty_source("ADD");
}

#[test]
fn runtime_failure_rolls_back_the_entire_request_at_both_doors() {
    for scoped in [false, true] {
        let (k, reader) = over(DurableStore::in_memory().unwrap());
        update(&k, "INSERT DATA { GRAPH <urn:g> { <urn:old> <urn:p> 0 } }").unwrap();
        let before = quads(&reader);
        let text = "DELETE DATA { GRAPH <urn:g> { <urn:old> <urn:p> 0 } }; INSERT DATA { GRAPH <urn:g> { <urn:new> <urn:p> 1 } }; CREATE GRAPH <urn:g>";
        let result = if scoped {
            scoped_update(&k, text)
        } else {
            update(&k, text)
        };
        assert!(result.is_err(), "scoped={scoped}: {result:?}");
        assert_eq!(quads(&reader), before, "scoped={scoped}");
        assert_eq!(graphs(&k), BTreeSet::from(["urn:g".to_owned()]));
    }
}

#[test]
fn graph_transfers_have_exact_set_contents_and_self_transfers_are_identity() {
    for operation in ["COPY", "MOVE", "ADD"] {
        for (source, destination) in [
            ("DEFAULT", "GRAPH <urn:b>"),
            ("GRAPH <urn:a>", "DEFAULT"),
            ("GRAPH <urn:a>", "GRAPH <urn:b>"),
        ] {
            let (k, reader) = over(DurableStore::in_memory().unwrap());
            update(&k, "INSERT DATA { <urn:d> <urn:p> 0 . GRAPH <urn:a> { <urn:a> <urn:p> 1 } GRAPH <urn:b> { <urn:b> <urn:p> 2 } }").unwrap();
            let before = quads(&reader);
            update(&k, &format!("{operation} {source} TO {source}")).unwrap();
            assert_eq!(quads(&reader), before);
            update(&k, &format!("{operation} {source} TO {destination}")).unwrap();
            let source_name = if source == "DEFAULT" {
                None
            } else {
                Some("urn:a")
            };
            let destination_name = if destination == "DEFAULT" {
                None
            } else {
                Some("urn:b")
            };
            let mut expected = BTreeSet::new();
            for (s, n, graph) in [
                ("d", 0, None),
                ("a", 1, Some("urn:a")),
                ("b", 2, Some("urn:b")),
            ] {
                if !(operation == "MOVE" && graph == source_name
                    || operation != "ADD" && graph == destination_name)
                {
                    expected.insert(quad_text(s, n, graph));
                }
                if graph == source_name {
                    expected.insert(quad_text(s, n, destination_name));
                }
            }
            assert_eq!(
                quads(&reader),
                expected,
                "{operation} {source} TO {destination}"
            );
            let mut expected_graphs = BTreeSet::from(["urn:a".to_owned(), "urn:b".to_owned()]);
            if operation == "MOVE" {
                if let Some(g) = source_name {
                    expected_graphs.remove(g);
                }
            }
            assert_eq!(graphs(&k), expected_graphs);
        }
    }
}
fn quad_text(subject: &str, n: u8, graph: Option<&str>) -> String {
    format!(
        "<urn:{subject}> <urn:p> \"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>{}",
        graph.map(|g| format!(" <{g}>")).unwrap_or_default()
    )
}

#[test]
fn scoped_counts_listings_and_data_follow_insert_delete_clear_and_drop() {
    let k = kernel();
    let insert = "INSERT DATA { GRAPH <urn:g> { <urn:s> <urn:p> 1 } }";
    assert_eq!(
        scoped_update(&k, insert).unwrap(),
        "updated <urn:g>: +1 -0 quads\n"
    );
    assert_eq!(
        scoped_update(&k, insert).unwrap(),
        "updated <urn:g>: +0 -0 quads\n"
    );
    assert!(issue(
        &k,
        Verb::Source,
        "urn:iki:store:graph-info",
        &[("graph", "urn:g")]
    )
    .unwrap()
    .contains("exists: true\nquads: 1\n"));
    assert_eq!(scoped_update(&k, "WITH <urn:g> DELETE { ?s <urn:p> ?n } INSERT { ?s <urn:p> ?next } WHERE { ?s <urn:p> ?n BIND(?n + 1 AS ?next) }").unwrap(), "updated <urn:g>: +1 -1 quads\n");
    assert!(ask(&k, "ASK { GRAPH <urn:g> { <urn:s> <urn:p> 2 } }"));
    assert_eq!(
        scoped_update(&k, "DELETE DATA { GRAPH <urn:g> { <urn:s> <urn:p> 2 } }").unwrap(),
        "updated <urn:g>: +0 -1 quads\n"
    );
    scoped_update(&k, insert).unwrap();
    assert_eq!(
        scoped_update(&k, "CLEAR GRAPH <urn:g>").unwrap(),
        "updated <urn:g>: +0 -1 quads\n"
    );
    assert!(graphs(&k).contains("urn:g"));
    assert!(issue(
        &k,
        Verb::Source,
        "urn:iki:store:graph-info",
        &[("graph", "urn:g")]
    )
    .unwrap()
    .contains("exists: true\nquads: 0\n"));
    scoped_update(&k, "DROP GRAPH <urn:g>").unwrap();
    assert!(graphs(&k).is_empty());
}

#[test]
fn insert_data_blank_nodes_are_shared_within_a_request_and_fresh_between_requests() {
    for scoped in [false, true] {
        let (k, reader) = over(DurableStore::in_memory().unwrap());
        let text = "INSERT DATA { GRAPH <urn:g> { _:x <urn:p> 1; <urn:q> 2 } }";
        for _ in 0..2 {
            if scoped {
                scoped_update(&k, text).unwrap();
            } else {
                update(&k, text).unwrap();
            }
        }
        let subjects: BTreeSet<_> = reader
            .snapshot_iter()
            .map(|q| q.unwrap().subject.to_string())
            .collect();
        assert_eq!(subjects.len(), 2);
        assert_eq!(quads(&reader).len(), 4);
        assert!(ask(&k, "ASK { GRAPH <urn:g> { ?x <urn:p> 1; <urn:q> 2 . ?y <urn:p> 1; <urn:q> 2 FILTER(?x != ?y) } }"));
    }
}

#[test]
fn clear_and_drop_default_named_and_all_preserve_exact_graph_registration() {
    for operation in ["CLEAR", "DROP"] {
        for target in ["DEFAULT", "NAMED", "ALL", "GRAPH <urn:a>"] {
            let (k, reader) = over(DurableStore::in_memory().unwrap());
            update(&k, "INSERT DATA { <urn:d> <urn:p> 0 . GRAPH <urn:a> { <urn:a> <urn:p> 1 } GRAPH <urn:b> { <urn:b> <urn:p> 2 } }; CREATE GRAPH <urn:empty>").unwrap();
            update(&k, &format!("{operation} {target}")).unwrap();
            let mut expected = BTreeSet::new();
            if !matches!(target, "DEFAULT" | "ALL") {
                expected.insert(quad_text("d", 0, None));
            }
            if !matches!(target, "NAMED" | "ALL" | "GRAPH <urn:a>") {
                expected.insert(quad_text("a", 1, Some("urn:a")));
            }
            if !matches!(target, "NAMED" | "ALL") {
                expected.insert(quad_text("b", 2, Some("urn:b")));
            }
            assert_eq!(quads(&reader), expected, "{operation} {target}");
            let mut expected_graphs = BTreeSet::from([
                "urn:a".to_owned(),
                "urn:b".to_owned(),
                "urn:empty".to_owned(),
            ]);
            if operation == "DROP" {
                if matches!(target, "NAMED" | "ALL") {
                    expected_graphs.clear();
                }
                if target == "GRAPH <urn:a>" {
                    expected_graphs.remove("urn:a");
                }
            }
            assert_eq!(graphs(&k), expected_graphs, "{operation} {target}");
            // The default graph remains writable after DROP DEFAULT / DROP ALL.
            update(&k, "INSERT DATA { <urn:after> <urn:p> 3 }").unwrap();
            assert!(ask(&k, "ASK { <urn:after> <urn:p> 3 }"));
        }
    }
}

#[test]
fn nested_scoped_reads_answer_independently_of_an_outside_graphs_existence() {
    for outside_exists in [false, true] {
        let k = kernel();
        update(&k, "INSERT DATA { GRAPH <urn:g> { <urn:s> <urn:p> 1 } }").unwrap();
        if outside_exists {
            update(
                &k,
                "INSERT DATA { GRAPH <urn:outside> { <urn:secret> <urn:p> 2 } }",
            )
            .unwrap();
        }
        // A variable graph is evaluated over the restricted dataset: EXISTS is false,
        // and OPTIONAL leaves the outside value unbound without losing the left row.
        let query = "SELECT ?s ?secret (EXISTS { GRAPH ?outside { ?x ?p ?o } } AS ?exists) WHERE { VALUES ?outside { <urn:outside> } ?s <urn:p> 1 OPTIONAL { GRAPH ?outside { ?secret <urn:p> ?n } } }";
        let json = issue(
            &k,
            Verb::Source,
            "urn:iki:store:graph-select",
            &[("graph", "urn:g"), ("query", query)],
        )
        .unwrap();
        let answer: serde_json::Value = serde_json::from_str(&json).unwrap();
        let rows = answer["results"]["bindings"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{json}");
        assert_eq!(rows[0]["s"]["value"], "urn:s");
        assert_eq!(rows[0]["exists"]["value"], "false");
        assert!(rows[0].get("secret").is_none());
        let receipt = scoped_update(&k, "INSERT { GRAPH <urn:g> { <urn:s> <urn:exists> ?exists } } WHERE { VALUES ?outside { <urn:outside> } BIND(EXISTS { GRAPH ?outside { ?s ?p ?o } } AS ?exists) }").unwrap();
        assert_eq!(receipt, "updated <urn:g>: +1 -0 quads\n");
        assert!(ask(
            &k,
            "ASK { GRAPH <urn:g> { <urn:s> <urn:exists> false } }"
        ));
        // The documented policy rejects explicitly named outside graphs on syntax alone.
        let error = issue(
            &k,
            Verb::Source,
            "urn:iki:store:graph-select",
            &[
                ("graph", "urn:g"),
                (
                    "query",
                    "SELECT * WHERE { FILTER EXISTS { GRAPH <urn:outside> { ?s ?p ?o } } }",
                ),
            ],
        )
        .unwrap_err();
        assert!(matches!(error, ikigai_core::Error::InvalidArgument { .. }));
    }
}

#[cfg(feature = "persistent")]
#[test]
fn committed_requests_and_rdf_terms_survive_reopening_without_an_explicit_flush() {
    let path = std::env::temp_dir().join(format!(
        "ikigai-store-answers-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir(&path).unwrap();
    let (expected, expected_graphs) = {
        let (k, reader) = over(DurableStore::open(&path).unwrap());
        update(
            &k,
            r#"INSERT DATA {
            <urn:s> <urn:typed> "0007"^^<http://www.w3.org/2001/XMLSchema#integer>;
                <urn:custom> "0007"^^<urn:datatype>;
                <urn:lang> "café 日本語"@fr;
                <urn:bytes> "line\nquote\"slash\\" .
            GRAPH <urn:g> { _:x <urn:p> "value"; <urn:q> _:x }
        }; CREATE GRAPH <urn:empty>"#,
        )
        .unwrap();
        #[cfg(feature = "rdf-12")]
        update(
            &k,
            r#"INSERT DATA { GRAPH <urn:g> {
            <urn:s> <urn:direction> "مرحبا"@ar--rtl;
                <urn:triple> <<( <urn:a> <urn:p> "object"@en )>> .
        } }"#,
        )
        .unwrap();
        // A later failed request must not replace the last committed state on reopen.
        assert!(update(
            &k,
            "CLEAR ALL; CREATE GRAPH <urn:empty>; CREATE GRAPH <urn:empty>"
        )
        .is_err());
        (quads(&reader), graphs(&k))
    };
    {
        let (k, reader) = over(DurableStore::open(&path).unwrap());
        assert_eq!(quads(&reader), expected);
        assert_eq!(graphs(&k), expected_graphs);
        assert!(issue(
            &k,
            Verb::Source,
            "urn:iki:store:graph-info",
            &[("graph", "urn:empty")]
        )
        .unwrap()
        .contains("exists: true\nquads: 0\n"));
        scoped_update(&k, "INSERT DATA { GRAPH <urn:g> { <urn:last> <urn:p> 9 } }").unwrap();
    }
    {
        let k = over(DurableStore::open(&path).unwrap()).0;
        assert!(ask(&k, "ASK { GRAPH <urn:g> { <urn:last> <urn:p> 9 } }"));
    }
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn default_graph_data_updates_are_sets_and_where_uses_the_preoperation_state() {
    let (k, reader) = over(DurableStore::in_memory().unwrap());
    update(
        &k,
        "INSERT DATA { <urn:s> <urn:p> 1, 2 }; INSERT DATA { <urn:s> <urn:p> 1 }",
    )
    .unwrap();
    assert_eq!(
        quads(&reader),
        BTreeSet::from([quad_text("s", 1, None), quad_text("s", 2, None)])
    );
    update(&k, "DELETE { <urn:s> <urn:p> ?n } INSERT { <urn:s> <urn:p> ?next } WHERE { <urn:s> <urn:p> ?n BIND(?n + 1 AS ?next) }").unwrap();
    // Both old values are deleted before either new value is inserted: 2 must survive.
    assert_eq!(
        quads(&reader),
        BTreeSet::from([quad_text("s", 2, None), quad_text("s", 3, None)])
    );
    update(&k, "DELETE DATA { <urn:s> <urn:p> 2, 8 }").unwrap();
    assert_eq!(quads(&reader), BTreeSet::from([quad_text("s", 3, None)]));
    update(&k, "DELETE WHERE { ?s ?p ?o }").unwrap();
    assert!(quads(&reader).is_empty());
}
