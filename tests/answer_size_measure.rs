//! The measurement behind the answer-size bound (ledger #970), kept so it can be re-run.
//!
//!     cargo test --release --test answer_size_measure -- --ignored --nocapture
//!     DATASET=/path/gonk-store.nq cargo test --release --features persistent \
//!         --test answer_size_measure -- --ignored --nocapture
//!
//! `measure_cross_product` is the claim: how many rows and bytes an unbounded three-pattern
//! cross product produces and how fast (counted into a sink, so the measurement itself holds
//! nothing), and how soon the bounded door refuses the same query.
//!
//! `measure_backup` is the cost the bound puts on gonk: its backup query (every quad, sorted,
//! SPARQL JSON) through `urn:iki:store:select` under the capability its job holds, without and
//! with answer grants. `DATASET` names an N-Quads file (a gonk backup, gunzipped); with
//! `--features persistent` it is loaded into a RocksDB store in a temporary directory, the
//! backing gonk runs on. Nothing from the dataset is printed but counts and timings.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_store::budget::{cap_answer, cap_answer_bytes, cap_budget};
use ikigai_store::{space, DurableStore, CAP_READ};
use oxigraph::sparql::results::{QueryResultsFormat, QueryResultsSerializer};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::sync::Arc;
use std::time::Instant;

/// Counts what is written to it and keeps none of it.
struct Count(u64);

impl std::io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn select(kernel: &Kernel, cap: &Capability, query: &str) -> ikigai_core::Result<usize> {
    block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
                .with_arg(
                    "as",
                    ArgRef::Inline(b"application/sparql-results+json".to_vec()),
                ),
            cap,
        ),
    )
    .map(|rep| rep.bytes.len())
}

#[test]
#[ignore]
fn measure_cross_product() {
    // The time-budget arc's graph: 60 nodes, two `<urn:p>` edges each, 120 quads.
    let mut triples = String::new();
    for i in 0..60usize {
        for j in [i + 1, i * 7 + 3] {
            triples.push_str(&format!("<urn:n{i}> <urn:p> <urn:n{}> . ", j % 60));
        }
    }
    let store = DurableStore::in_memory().unwrap();
    let kernel = Kernel::new(Arc::new(space(store)));
    block_on(kernel.issue(
        Request::new(Verb::Sink, Iri::parse("urn:iki:store:update").unwrap()).with_arg(
            "content",
            ArgRef::Inline(format!("INSERT DATA {{ {triples} }}").into_bytes()),
        ),
        &Capability::root(),
    ))
    .unwrap();
    let raw = oxigraph::store::Store::new().unwrap();
    raw.update(&format!("INSERT DATA {{ {triples} }}")).unwrap();
    for query in [
        "SELECT ?a WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }",
        "SELECT * WHERE { ?a ?b ?c . ?d ?e ?f . ?g ?h ?i }",
    ] {
        // Unbounded, beneath the doors.
        let t = Instant::now();
        let QueryResults::Solutions(solutions) = SparqlEvaluator::new()
            .parse_query(query)
            .unwrap()
            .on_store(&raw)
            .execute()
            .unwrap()
        else {
            unreachable!()
        };
        let mut serializer = QueryResultsSerializer::from_format(QueryResultsFormat::Json)
            .serialize_solutions_to_writer(Count(0), solutions.variables().to_vec())
            .unwrap();
        let mut rows = 0u64;
        for s in solutions {
            serializer.serialize(&s.unwrap()).unwrap();
            rows += 1;
        }
        let bytes = serializer.finish().unwrap().0;
        println!(
            "UNBOUNDED `{query}` ({} B): {rows} rows, {bytes} bytes of JSON, {} ms",
            query.len(),
            t.elapsed().as_millis()
        );
        // Through the door, under the base.
        let t = Instant::now();
        let got = select(&kernel, &Capability::scoped([CAP_READ]), query);
        println!(
            "BOUNDED   `{query}`: {} after {} ms",
            match got {
                Ok(n) => format!("ANSWERED {n} bytes"),
                Err(e) => format!("refused ({e})"),
            },
            t.elapsed().as_millis()
        );
    }
}

#[test]
#[ignore]
fn measure_backup() {
    let Ok(path) = std::env::var("DATASET") else {
        println!("set DATASET to an N-Quads file");
        return;
    };
    #[cfg(feature = "persistent")]
    let (store, _dir) = {
        let dir = std::env::temp_dir().join(format!("ikigai-store-970-{}", std::process::id()));
        (DurableStore::open(&dir).unwrap(), Dir(dir))
    };
    #[cfg(not(feature = "persistent"))]
    let store = DurableStore::in_memory().unwrap();
    println!("BACKING {:?}", store.backing());
    let kernel = Kernel::new(Arc::new(space(store)));
    let t = Instant::now();
    block_on(
        kernel.issue(
            Request::new(Verb::Sink, Iri::parse("urn:iki:store:load").unwrap())
                .with_arg("content", ArgRef::Inline(std::fs::read(&path).unwrap()))
                .with_arg("format", ArgRef::Inline(b"application/n-quads".to_vec())),
            &Capability::root(),
        ),
    )
    .unwrap();
    println!("LOADED in {} ms", t.elapsed().as_millis());
    let backup = "SELECT ?g ?s ?p ?o WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } } \
                  ORDER BY ?g ?s ?p ?o";
    let count = "SELECT (COUNT(*) AS ?n) WHERE { { GRAPH ?g { ?s ?p ?o } } UNION { ?s ?p ?o } }";
    let counted = block_on(
        kernel.issue(
            Request::new(Verb::Source, Iri::parse("urn:iki:store:select").unwrap())
                .with_arg("query", ArgRef::Inline(count.as_bytes().to_vec()))
                .with_arg("as", ArgRef::Inline(b"text/csv".to_vec())),
            &Capability::root(),
        ),
    )
    .unwrap();
    println!(
        "QUADS {}",
        String::from_utf8_lossy(&counted.bytes)
            .trim()
            .replace("\r\n", " ")
    );
    // gonk's backup job today: `backup::JOB_SCOPES` = backup, read, a 120 s budget grant.
    let job = vec![
        "urn:cap:gonk:backup".to_string(),
        CAP_READ.to_string(),
        cap_budget(120_000),
    ];
    let mut with_grants = job.clone();
    with_grants.push(cap_answer(10_000_000));
    with_grants.push(cap_answer_bytes(1 << 30));
    for (label, cap) in [
        ("job scopes as today", Capability::scoped(job)),
        (
            "job scopes + answer grants",
            Capability::scoped(with_grants),
        ),
        ("root", Capability::root()),
    ] {
        for _ in 0..2 {
            let t = Instant::now();
            let got = select(&kernel, &cap, backup);
            println!(
                "BACKUP {label}: {} after {} ms",
                match got {
                    Ok(n) => format!("ANSWERED {n} bytes"),
                    Err(e) => format!("refused ({e})"),
                },
                t.elapsed().as_millis()
            );
        }
    }
}

/// Removes the temporary RocksDB directory when the measurement ends.
#[cfg(feature = "persistent")]
struct Dir(std::path::PathBuf);

#[cfg(feature = "persistent")]
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
