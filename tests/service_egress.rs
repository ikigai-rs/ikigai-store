//! `SERVICE` never reaches the network through this store, in any build (ledger #1083, #145).
//!
//! The claim: when any crate in a host's graph enables `oxigraph/http-client` (rudof_rdf does,
//! on every native target, so `ikigai-cli` and `ikigai-web-demo` both have it), oxigraph's
//! `SparqlEvaluator` installs a default HTTP service handler, and `SERVICE <http://…>` in a
//! caller's query becomes an outbound request with no `urn:cap:net:*` anywhere near it. This
//! crate cannot turn the feature off, and `without_default_http_service_handler` is itself
//! behind the feature, so it cannot be called unconditionally.
//!
//! ★ Reproduced here with this crate's own `http-client` feature (`--features http-client`, or
//! CI's `features: "*"`), which enables exactly `oxigraph/http-client` — the same switch the
//! hosts get by unification. The CONTROL (raw oxigraph reaches the stub) is cfg'd on it, the
//! sound direction: the feature on implies the handler is installed. The REFUSALS are not
//! cfg'd on anything, because a host reaching the feature through rudof never enables this
//! crate's — so they run in every build, and are the reproduction in a build with it.
//!
//! Every request goes to a stub on 127.0.0.1 with an ephemeral port, never a real host.

use futures::executor::block_on;
use ikigai_core::{ArgRef, Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_store::{space, DurableStore};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// A plain-HTTP stub that records the request line of every connection it is sent, and
/// answers each with an empty SPARQL result set (so a client that does reach it finishes).
struct Stub {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

/// The first bytes of the connection [`Stub::requests`] makes itself. Accepts are served in
/// backlog order, so once the stub has answered this one, every connection made before it has
/// been recorded: the negative assertions need no sleep and cannot race.
const SENTINEL: &[u8] = b"SENTINEL\r\n";

impl Stub {
    fn start() -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let head = read_head(&mut stream);
                if head.as_bytes().starts_with(SENTINEL) {
                    let _ = stream.write_all(b"ok");
                    continue;
                }
                record
                    .lock()
                    .unwrap()
                    .push(head.lines().next().unwrap_or("").to_string());
                let body = r#"{"head":{"vars":["s"]},"results":{"bindings":[]}}"#;
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Stub { addr, seen }
    }

    fn url(&self) -> String {
        format!("http://{}/sparql", self.addr)
    }

    /// Every request line the stub has been sent, after a sentinel round trip.
    fn requests(&self) -> Vec<String> {
        let mut sentinel = TcpStream::connect(self.addr).unwrap();
        sentinel.write_all(SENTINEL).unwrap();
        let mut ack = Vec::new();
        sentinel.read_to_end(&mut ack).unwrap();
        assert_eq!(ack, b"ok", "the stub did not answer its sentinel");
        self.seen.lock().unwrap().clone()
    }
}

/// Read up to the end of the request headers (or the sentinel line), enough to log it.
fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
        head.push(byte[0]);
        if head == SENTINEL || head.len() > 64 * 1024 {
            break;
        }
        if head.ends_with(b"\r\n\r\n") {
            // Drain a body too, or closing with it unread resets the client's connection.
            let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
            let length = text
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|n| n.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            let _ = stream.read_exact(&mut body);
            break;
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

fn kernel() -> Kernel {
    Kernel::new(Arc::new(space(DurableStore::in_memory().unwrap())))
}

fn issue(kernel: &Kernel, iri: &str, args: &[(&str, &str)]) -> ikigai_core::Result<String> {
    let verb = if iri.ends_with("update") {
        Verb::Sink
    } else {
        Verb::Source
    };
    let mut req = Request::new(verb, Iri::parse(iri).unwrap());
    for (name, value) in args {
        req = req.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    block_on(kernel.issue(req, &Capability::root()))
        .map(|rep| String::from_utf8_lossy(&rep.bytes).into_owned())
}

/// One request at one door: the IRI, its arguments, and the argument a refusal must name.
type DoorRequest = (&'static str, Vec<(&'static str, String)>, &'static str);

/// Every door that evaluates caller SPARQL, with a `SERVICE` to `url` in a shape that door
/// accepts, and the argument a refusal must name.
fn service_requests(url: &str) -> Vec<DoorRequest> {
    let graph = "urn:g";
    let select = format!("SELECT ?s WHERE {{ SERVICE <{url}> {{ ?s ?p ?o }} }}");
    let silent = format!("SELECT ?s WHERE {{ SERVICE SILENT <{url}> {{ ?s ?p ?o }} }}");
    let ask = format!("ASK {{ SERVICE <{url}> {{ ?s ?p ?o }} }}");
    let construct = format!("CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <{url}> {{ ?s ?p ?o }} }}");
    let describe = format!("DESCRIBE ?s WHERE {{ SERVICE <{url}> {{ ?s ?p ?o }} }}");
    // SERVICE hidden one level down, where only a walk of the whole algebra would see it.
    let nested = format!(
        "SELECT ?x WHERE {{ BIND(1 AS ?x) FILTER EXISTS {{ OPTIONAL {{ SERVICE <{url}> {{ ?s ?p ?o }} }} }} }}"
    );
    // A variable service name: the IRI arrives as a binding, so no textual check could see it.
    let variable = format!(
        "SELECT ?s WHERE {{ BIND(<{url}> AS ?endpoint) LATERAL {{ SERVICE ?endpoint {{ ?s ?p ?o }} }} }}"
    );
    let update =
        format!("INSERT {{ <urn:s> <urn:p> ?o }} WHERE {{ SERVICE <{url}> {{ ?s ?p ?o }} }}");
    let scoped_update = format!(
        "INSERT {{ GRAPH <{graph}> {{ <urn:s> <urn:p> ?o }} }} WHERE {{ SERVICE <{url}> {{ ?s ?p ?o }} }}"
    );
    vec![
        (
            "urn:iki:store:select",
            vec![("query", select.clone())],
            "query",
        ),
        (
            "urn:iki:store:select",
            vec![("query", silent.clone())],
            "query",
        ),
        (
            "urn:iki:store:select",
            vec![("query", nested.clone())],
            "query",
        ),
        (
            "urn:iki:store:select",
            vec![("query", variable.clone())],
            "query",
        ),
        ("urn:iki:store:ask", vec![("query", ask.clone())], "query"),
        (
            "urn:iki:store:construct",
            vec![("query", construct.clone())],
            "query",
        ),
        (
            "urn:iki:store:describe",
            vec![("query", describe.clone())],
            "query",
        ),
        (
            "urn:iki:store:graph-select",
            vec![("graph", graph.into()), ("query", select)],
            "query",
        ),
        (
            "urn:iki:store:graph-select",
            vec![("graph", graph.into()), ("query", silent)],
            "query",
        ),
        (
            "urn:iki:store:graph-ask",
            vec![("graph", graph.into()), ("query", ask)],
            "query",
        ),
        (
            "urn:iki:store:graph-construct",
            vec![("graph", graph.into()), ("query", construct)],
            "query",
        ),
        (
            "urn:iki:store:graph-describe",
            vec![("graph", graph.into()), ("query", describe)],
            "query",
        ),
        ("urn:iki:store:update", vec![("content", update)], "content"),
        (
            "urn:iki:store:graph-update",
            vec![("graph", graph.into()), ("content", scoped_update)],
            "content",
        ),
    ]
}

/// ★ The fix, at every door: refused as a typed `InvalidArgument` naming the argument and
/// `SERVICE`, and NOTHING reaches the stub. In a build with `oxigraph/http-client` on this is
/// the reproduction (before the fix, each of these was a request to the stub); in a default
/// build it proves the refusal did not regress anything that already failed.
#[test]
fn service_is_refused_at_every_door_and_nothing_reaches_the_network() {
    let stub = Stub::start();
    let k = kernel();
    for (iri, args, arg) in service_requests(&stub.url()) {
        let args: Vec<(&str, &str)> = args.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let err = issue(&k, iri, &args).expect_err(&format!("{iri} answered: {args:?}"));
        assert!(
            matches!(&err, Error::InvalidArgument { name, detail }
                if name == arg && detail.contains("SERVICE")),
            "{iri} {args:?}: not a typed SERVICE refusal: {err}"
        );
    }
    assert_eq!(
        stub.requests(),
        Vec::<String>::new(),
        "a SERVICE clause reached the network through this store"
    );
}

/// The refusal names no capability as a remedy: no grant opens `SERVICE` here, so a `Denied`
/// naming `urn:cap:net:*` would send a caller looking for a grant that changes nothing.
#[test]
fn the_refusal_says_where_remote_data_comes_in_instead() {
    let stub = Stub::start();
    let err = issue(
        &kernel(),
        "urn:iki:store:select",
        &[(
            "query",
            &format!(
                "SELECT ?s WHERE {{ SERVICE <{}> {{ ?s ?p ?o }} }}",
                stub.url()
            ),
        )],
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("urn:iki:store:load"), "{err}");
    assert!(err.contains("Nothing was evaluated"), "{err}");
}

/// A query with no SERVICE in it still answers at every door kind: the refusal is not a
/// blanket one.
#[test]
fn a_query_without_service_still_answers() {
    let k = kernel();
    issue(
        &k,
        "urn:iki:store:update",
        &[(
            "content",
            "INSERT DATA { <urn:s> <urn:p> <urn:o> GRAPH <urn:g> { <urn:s> <urn:p> <urn:o> } }",
        )],
    )
    .unwrap();
    let rows = issue(
        &k,
        "urn:iki:store:select",
        &[
            ("query", "SELECT ?o WHERE { <urn:s> <urn:p> ?o }"),
            ("as", "text/csv"),
        ],
    )
    .unwrap();
    assert!(rows.contains("urn:o"), "{rows}");
    let rows = issue(
        &k,
        "urn:iki:store:graph-select",
        &[
            ("graph", "urn:g"),
            ("query", "SELECT ?o WHERE { <urn:s> <urn:p> ?o }"),
            ("as", "text/csv"),
        ],
    )
    .unwrap();
    assert!(rows.contains("urn:o"), "{rows}");
}

// ------------------------------------------------------------- the other fetching paths

/// `LOAD <url>` is refused at the door (ledger #992), so it never reaches oxigraph's own HTTP
/// client — which the service handler does NOT govern: `LOAD` builds its client from the
/// evaluator's HTTP settings, not from the service registry. The door is the only guard.
#[test]
fn load_sends_nothing() {
    let stub = Stub::start();
    let k = kernel();
    let url = stub.url();
    for (iri, text) in [
        ("urn:iki:store:update", format!("LOAD <{url}>")),
        ("urn:iki:store:update", format!("LOAD SILENT <{url}>")),
        (
            "urn:iki:store:graph-update",
            format!("LOAD <{url}> INTO GRAPH <urn:g>"),
        ),
    ] {
        let mut args = vec![("content", text.as_str())];
        if iri.ends_with("graph-update") {
            args.push(("graph", "urn:g"));
        }
        let err = issue(&k, iri, &args).unwrap_err().to_string();
        assert!(err.contains("LOAD"), "{iri}: {err}");
    }
    assert_eq!(
        stub.requests(),
        Vec::<String>::new(),
        "LOAD reached the network"
    );
}

/// `FROM` / `FROM NAMED` name graphs IN the store; oxigraph never dereferences them. Asked of
/// the unscoped doors (the scoped ones refuse the clauses outright): the query answers, from
/// the store, and the stub sees nothing.
#[test]
fn from_and_from_named_are_not_dereferenced() {
    let stub = Stub::start();
    let k = kernel();
    let url = stub.url();
    for query in [
        format!("SELECT ?s FROM <{url}> WHERE {{ ?s ?p ?o }}"),
        format!("SELECT ?s FROM NAMED <{url}> WHERE {{ GRAPH ?g {{ ?s ?p ?o }} }}"),
        format!("CONSTRUCT {{ ?s ?p ?o }} FROM <{url}> WHERE {{ ?s ?p ?o }}"),
        format!("SELECT ?s WHERE {{ GRAPH <{url}> {{ ?s ?p ?o }} }}"),
    ] {
        issue(&k, "urn:iki:store:select", &[("query", &query)])
            .or_else(|_| issue(&k, "urn:iki:store:construct", &[("query", &query)]))
            .unwrap_or_else(|e| panic!("{query}: {e}"));
    }
    assert_eq!(
        stub.requests(),
        Vec::<String>::new(),
        "a dataset clause was dereferenced"
    );
}

// ------------------------------------------------------------------------ the control

/// ★ The control, and the reason the tests above are not vacuous in this build: with
/// `oxigraph/http-client` on, a `SparqlEvaluator` this crate did NOT build — raw oxigraph —
/// really does send `SERVICE` to the stub. If this ever stops reaching it (upstream moved the
/// default, the feature stopped switching it), the refusal tests above prove nothing in this
/// build, and this fails to say so.
#[cfg(feature = "http-client")]
#[test]
fn control_raw_oxigraph_with_http_client_reaches_the_stub() {
    use oxigraph::sparql::{QueryResults, SparqlEvaluator};
    let stub = Stub::start();
    let store = oxigraph::store::Store::new().unwrap();
    let query = format!(
        "SELECT ?s WHERE {{ SERVICE <{}> {{ ?s ?p ?o }} }}",
        stub.url()
    );
    let results = SparqlEvaluator::new()
        .parse_query(&query)
        .unwrap()
        .on_store(&store)
        .execute()
        .unwrap();
    let QueryResults::Solutions(solutions) = results else {
        panic!("not a solution set")
    };
    for solution in solutions {
        solution.unwrap();
    }
    let seen = stub.requests();
    assert_eq!(
        seen.len(),
        1,
        "raw oxigraph did not reach the stub: {seen:?}"
    );
}
