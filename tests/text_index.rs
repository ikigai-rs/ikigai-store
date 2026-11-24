//! The text index (ledger #4128): `urn:iki:store:graph-text` and the `text-index` resources.
#![cfg(feature = "text-index")]

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{
    cap_read_graph, cap_write_graph, graph_thread, space, DurableStore, TextEnrollment,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

const A: &str = "urn:example:ledger:alpha";
const B: &str = "urn:example:ledger:beta";
const GRAVE: &str = "urn:example:ledger:alpha:deleted";
const TITLE: &str = "urn:example:title";
const BODY: &str = "urn:example:body";

fn enrollment() -> TextEnrollment {
    TextEnrollment::new()
        .graphs_starting_with("urn:example:ledger:")
        .predicate_weighted(TITLE, 2.0)
        .predicate(BODY)
}

fn fixture() -> String {
    let mut nq = String::new();
    let lines = [
        (A, "1", TITLE, "The staff meeting ran late"),
        (
            A,
            "1",
            BODY,
            "The lease ran out before the staff could renew it.",
        ),
        (A, "2", TITLE, "Parser rewrite"),
        (
            A,
            "2",
            BODY,
            "A new parser for flagstaff and staffs, not the same word.",
        ),
        (A, "3", BODY, "Staff: the lease ran out, again."),
        (A, "4", BODY, "nothing to see here"),
        (B, "5", TITLE, "Staff onboarding"),
        (B, "5", BODY, "the parser is fine"),
        (GRAVE, "6", BODY, "staff graveyard text"),
    ];
    for (g, s, p, o) in lines {
        nq.push_str(&format!("<urn:item:{s}> <{p}> \"{o}\" <{g}> .\n"));
    }
    nq
}

fn kernel_over(store: DurableStore) -> Kernel {
    Kernel::new(Arc::new(space(store)))
}

fn seeded() -> Kernel {
    let store = DurableStore::in_memory().unwrap();
    let kernel = kernel_over(store.with_text_index(enrollment()).unwrap());
    load(&kernel, &fixture());
    wait_ready(&kernel, A);
    wait_ready(&kernel, B);
    kernel
}

fn load(kernel: &Kernel, nq: &str) {
    issue(
        kernel,
        Verb::Sink,
        "urn:iki:store:load",
        &[("content", nq), ("format", "application/n-quads")],
        &Capability::root(),
    )
    .expect("loading");
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

fn status_iri(graph: &str) -> String {
    let encoded = graph_thread(graph);
    let encoded = encoded.rsplit_once("graph-written:").unwrap().1;
    format!("urn:iki:store:text-index:graph:{encoded}")
}

fn status(kernel: &Kernel, graph: &str) -> String {
    issue(
        kernel,
        Verb::Source,
        &status_iri(graph),
        &[],
        &Capability::root(),
    )
    .expect("status")
}

fn wait_ready(kernel: &Kernel, graph: &str) {
    let start = Instant::now();
    while !status(kernel, graph).contains("state: ready") {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{graph} never became ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn all_read() -> Capability {
    Capability::scoped([A, B, GRAVE].iter().map(|g| cap_read_graph(g)))
}

/// `(subject, predicate, graph, score)` per hit.
fn search(
    kernel: &Kernel,
    graphs: &str,
    args: &[(&str, &str)],
    cap: &Capability,
) -> Result<Vec<(String, String, String, f64)>, Error> {
    let mut all = vec![("graph", graphs)];
    all.extend_from_slice(args);
    let body = issue(kernel, Verb::Source, "urn:iki:store:graph-text", &all, cap)?;
    let json: serde_json::Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(json["schema"], 1);
    Ok(json["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            let keys: BTreeSet<&str> = h.as_object().unwrap().keys().map(String::as_str).collect();
            assert_eq!(
                keys,
                BTreeSet::from(["graph", "predicate", "score", "subject"]),
                "index-neutral hit"
            );
            (
                h["subject"].as_str().unwrap().to_string(),
                h["predicate"].as_str().unwrap().to_string(),
                h["graph"].as_str().unwrap().to_string(),
                h["score"].as_f64().unwrap(),
            )
        })
        .collect())
}

fn pairs(hits: &[(String, String, String, f64)]) -> BTreeSet<(String, String)> {
    hits.iter()
        .map(|(s, p, _, _)| (s.clone(), p.clone()))
        .collect()
}

/// The scan: the same question as a REGEX over the store, through `graph-select`.
fn scan(kernel: &Kernel, graph: &str, regex: &str) -> BTreeSet<(String, String)> {
    let query = format!(
        "SELECT ?s ?p WHERE {{ VALUES ?p {{ <{TITLE}> <{BODY}> }} ?s ?p ?o FILTER(REGEX(STR(?o), \"{regex}\", \"i\")) }}"
    );
    let csv = issue(
        kernel,
        Verb::Source,
        "urn:iki:store:graph-select",
        &[("graph", graph), ("query", &query), ("as", "text/csv")],
        &all_read(),
    )
    .expect("scan");
    csv.lines()
        .skip(1)
        .map(|l| {
            let (s, p) = l.trim_end().split_once(',').unwrap();
            (s.to_string(), p.to_string())
        })
        .collect()
}

#[test]
fn index_answers_equal_the_scan_for_words_and_phrases() {
    let kernel = seeded();
    for word in ["staff", "parser", "lease", "nothing"] {
        let hits = search(&kernel, A, &[("words", word)], &all_read()).unwrap();
        assert_eq!(
            pairs(&hits),
            scan(&kernel, A, &format!("\\\\b{word}\\\\b")),
            "words={word}"
        );
    }
    let hits = search(&kernel, A, &[("phrase", "lease ran out")], &all_read()).unwrap();
    assert_eq!(
        pairs(&hits),
        scan(&kernel, A, "\\\\blease\\\\W+ran\\\\W+out\\\\b")
    );
    assert_eq!(pairs(&hits).len(), 2);
    let hits = search(&kernel, A, &[("words", "staff lease")], &all_read()).unwrap();
    assert_eq!(
        pairs(&hits).len(),
        2,
        "mode=all needs both words in one literal: {hits:?}"
    );
    let hits = search(
        &kernel,
        A,
        &[("words", "staff parser"), ("mode", "any")],
        &all_read(),
    )
    .unwrap();
    assert_eq!(pairs(&hits).len(), 5, "{hits:?}");
}

#[test]
fn a_title_match_weighs_twice_and_hits_are_ranked() {
    let kernel = seeded();
    let hits = search(&kernel, A, &[("words", "staff")], &all_read()).unwrap();
    assert!(
        hits.windows(2).all(|w| w[0].3 >= w[1].3),
        "most relevant first"
    );
    assert_eq!(hits[0].1, TITLE, "the title match ranks first: {hits:?}");
    let only_body = search(
        &kernel,
        A,
        &[("words", "staff"), ("predicate", BODY)],
        &all_read(),
    )
    .unwrap();
    assert!(only_body.iter().all(|h| h.1 == BODY));
}

#[test]
fn a_search_naming_an_ungranted_graph_is_refused_never_answered_over_the_subset() {
    let kernel = seeded();
    let only_a = Capability::scoped([cap_read_graph(A)]);
    let err = search(&kernel, &format!("{A} {B}"), &[("words", "staff")], &only_a).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    assert!(
        err.to_string().contains(&cap_read_graph(B)),
        "names the missing grant: {err}"
    );
    assert!(search(&kernel, A, &[("words", "staff")], &only_a).is_ok());
    let broad = Capability::scoped([ikigai_store::CAP_READ.to_string()]);
    assert!(
        search(&kernel, A, &[("words", "staff")], &broad).is_err(),
        "the broad grant is not a graph grant"
    );
}

#[test]
fn a_union_ranks_over_the_named_graphs_only() {
    let kernel = seeded();
    let alone = search(&kernel, A, &[("words", "parser")], &all_read()).unwrap();
    let union = search(
        &kernel,
        &format!("{A} {B}"),
        &[("words", "parser")],
        &all_read(),
    )
    .unwrap();
    assert!(union.iter().any(|h| h.2 == B) && union.iter().any(|h| h.2 == A));
    // Filling B with the word changes A's score only in a union that names B.
    let mut more = String::new();
    for i in 0..20 {
        more.push_str(&format!(
            "<urn:item:b{i}> <{BODY}> \"parser parser\" <{B}> .\n"
        ));
    }
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", B),
            (
                "content",
                &format!(
                    "INSERT DATA {{ GRAPH <{B}> {{ {} }} }}",
                    more.replace(&format!(" <{B}> ."), " .")
                ),
            ),
        ],
        &Capability::scoped([cap_write_graph(B)]),
    )
    .expect("writing B");
    let again = search(&kernel, A, &[("words", "parser")], &all_read()).unwrap();
    assert_eq!(alone, again, "a search of A alone says nothing about B");
}

#[test]
fn a_graph_without_an_index_answers_no_index_and_graveyards_are_not_enrolled() {
    let kernel = seeded();
    let err = search(&kernel, GRAVE, &[("words", "staff")], &all_read()).unwrap_err();
    assert!(matches!(err, Error::Unavailable(_)), "{err:?}");
    assert!(err.to_string().contains("no index"), "{err}");
    assert!(status(&kernel, GRAVE).contains("state: not enrolled"));
    let enrolled = issue(
        &kernel,
        Verb::Source,
        "urn:iki:store:text-enrollment",
        &[],
        &all_read(),
    )
    .unwrap();
    assert!(
        enrolled.contains(&format!("predicate: <{TITLE}> weight 2")),
        "{enrolled}"
    );
}

#[test]
fn a_scoped_write_is_searchable_at_once_and_a_broad_update_rebuilds() {
    let kernel = seeded();
    let write = Capability::scoped([cap_write_graph(A), cap_read_graph(A)]);
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", A),
            (
                "content",
                &format!(
                    "INSERT DATA {{ GRAPH <{A}> {{ <urn:item:9> <{BODY}> \"quokka sighting\" }} }}"
                ),
            ),
        ],
        &write,
    )
    .unwrap();
    let hits = search(&kernel, A, &[("words", "quokka")], &all_read()).unwrap();
    assert_eq!(
        pairs(&hits),
        BTreeSet::from([("urn:item:9".to_string(), BODY.to_string())])
    );
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", A),
            (
                "content",
                &format!(
                    "DELETE DATA {{ GRAPH <{A}> {{ <urn:item:9> <{BODY}> \"quokka sighting\" }} }}"
                ),
            ),
        ],
        &write,
    )
    .unwrap();
    assert!(search(&kernel, A, &[("words", "quokka")], &all_read())
        .unwrap()
        .is_empty());

    // A broad update computes no difference: every enrolled graph is rebuilt.
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:update",
        &[(
            "content",
            &format!("INSERT DATA {{ GRAPH <{B}> {{ <urn:item:10> <{BODY}> \"wombat\" }} }}"),
        )],
        &Capability::root(),
    )
    .unwrap();
    let state = status(&kernel, B);
    assert!(!state.contains("state: not enrolled"), "{state}");
    wait_ready(&kernel, B);
    let hits = search(&kernel, B, &[("words", "wombat")], &all_read()).unwrap();
    assert_eq!(hits.len(), 1, "the rebuild saw the broad write");
}

#[test]
fn rebuild_and_compact_need_the_write_grant_and_compact_drops_deleted_documents() {
    let kernel = seeded();
    let rebuild = format!("{}:rebuild", status_iri(A));
    let compact = format!("{}:compact", status_iri(A));
    let err = issue(&kernel, Verb::Sink, &compact, &[], &all_read()).unwrap_err();
    assert!(matches!(err, Error::Denied(_)), "{err:?}");
    let write = Capability::scoped([cap_write_graph(A), cap_read_graph(A)]);
    let rebuilt = issue(&kernel, Verb::Sink, &rebuild, &[], &write).unwrap();
    assert_eq!(rebuilt, format!("rebuilt <{A}>: 6 documents\n"));
    issue(
        &kernel,
        Verb::Sink,
        "urn:iki:store:graph-update",
        &[
            ("graph", A),
            (
                "content",
                &format!("DELETE WHERE {{ GRAPH <{A}> {{ <urn:item:4> ?p ?o }} }}"),
            ),
        ],
        &write,
    )
    .unwrap();
    issue(&kernel, Verb::Sink, &compact, &[], &write).unwrap();
    assert!(
        status(&kernel, A).contains("documents: 5\n"),
        "{}",
        status(&kernel, A)
    );
}

/// A purge's text leaves the index's files once compaction answers, and a reopened store
/// whose index was not closed cleanly rebuilds it.
#[cfg(feature = "persistent")]
#[test]
fn on_disk_compaction_removes_purged_text_and_an_unclean_index_is_rebuilt() {
    let dir = std::env::temp_dir().join(format!("ikigai-store-text-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let secret = "zqxjvsecretword";
    let write = Capability::scoped([cap_write_graph(A), cap_read_graph(A)]);
    {
        let kernel = kernel_over(
            DurableStore::open(&dir)
                .unwrap()
                .with_text_index(enrollment())
                .unwrap(),
        );
        load(&kernel, &fixture());
        wait_ready(&kernel, A);
        issue(
            &kernel,
            Verb::Sink,
            "urn:iki:store:graph-update",
            &[
                ("graph", A),
                (
                    "content",
                    &format!(
                        "INSERT DATA {{ GRAPH <{A}> {{ <urn:item:7> <{BODY}> \"{secret}\" }} }}"
                    ),
                ),
            ],
            &write,
        )
        .unwrap();
        assert_eq!(
            search(&kernel, A, &[("words", secret)], &all_read())
                .unwrap()
                .len(),
            1
        );

        issue(
            &kernel,
            Verb::Sink,
            "urn:iki:store:graph-update",
            &[
                ("graph", A),
                (
                    "content",
                    &format!(
                        "DELETE DATA {{ GRAPH <{A}> {{ <urn:item:7> <{BODY}> \"{secret}\" }} }}"
                    ),
                ),
            ],
            &write,
        )
        .unwrap();
        assert!(search(&kernel, A, &[("words", secret)], &all_read())
            .unwrap()
            .is_empty());
        let compact = format!("{}:compact", status_iri(A));
        issue(&kernel, Verb::Sink, &compact, &[], &write).unwrap();
        let after = status(&kernel, A);
        assert!(after.contains("awaiting compaction: 0\n"), "{after}");
        assert!(
            !index_bytes_contain(&dir, secret),
            "no segment file holds the purged word"
        );
    }
    // Closed cleanly: ready at once on reopen.
    {
        let kernel = kernel_over(
            DurableStore::open(&dir)
                .unwrap()
                .with_text_index(enrollment())
                .unwrap(),
        );
        assert!(
            status(&kernel, A).contains("state: ready"),
            "{}",
            status(&kernel, A)
        );
    }
    // Mark every index unclean, as a crash would leave it: reopened, it is rebuilt.
    for entry in std::fs::read_dir(dir.join("text-index")).unwrap() {
        let meta = entry.unwrap().path().join("ikigai-text-index.json");
        let text = std::fs::read_to_string(&meta)
            .unwrap()
            .replace("\"clean\":true", "\"clean\":false");
        std::fs::write(&meta, text).unwrap();
    }
    {
        let kernel = kernel_over(
            DurableStore::open(&dir)
                .unwrap()
                .with_text_index(enrollment())
                .unwrap(),
        );
        wait_ready(&kernel, A);
        let hits = search(&kernel, A, &[("words", "staff")], &all_read()).unwrap();
        assert_eq!(pairs(&hits), scan(&kernel, A, "\\\\bstaff\\\\b"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "persistent")]
fn index_bytes_contain(dir: &std::path::Path, needle: &str) -> bool {
    let mut stack = vec![dir.join("text-index")];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle.as_bytes())
            {
                return true;
            }
        }
    }
    false
}
