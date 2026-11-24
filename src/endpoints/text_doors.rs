//! The text-index resources (ledger #4128), bound only when the host enrolled graphs with
//! [`DurableStore::with_text_index`]:
//!
//! ```text
//! urn:iki:store:graph-text                      Source  ranked words/phrase search  urn:cap:store:read:graph:<iri> each
//! urn:iki:store:text-index:graph:{graph}        Source  one graph's index state     urn:cap:store:read:graph:<iri>
//! urn:iki:store:text-index:graph:{graph}:rebuild Sink   rebuild it from the store   urn:cap:store:write:graph:<iri>
//! urn:iki:store:text-index:graph:{graph}:compact Sink   merge away deleted text     urn:cap:store:write:graph:<iri>
//! urn:iki:store:text-enrollment                 Source  what the host indexes       urn:cap:store:read* (either)
//! ```
//!
//! ★ `graph-text` is gated exactly as `graph-select`: a request naming a graph the caller
//! holds no read grant for is refused, naming the grant, never answered over the graphs it
//! does hold. Its answer is cached under the same threads a `graph-select` of the same graphs
//! would be. A hit names subject, predicate, graph and score, and nothing of the engine.

use super::*;
use crate::text::{SearchError, TextIndexes, TextQuery};

const TEXT_INDEX_PREFIX: &str = "urn:iki:store:text-index:graph:";

/// The most hits one search answers, and the default.
const MAX_LIMIT: usize = 1000;
const DEFAULT_LIMIT: usize = 50;

pub(super) fn bind(space: EndpointSpace, store: &Arc<DurableStore>) -> EndpointSpace {
    let Some(text) = store.text().cloned() else {
        return space;
    };
    space
        .bind(
            Exact::new("urn:iki:store:graph-text"),
            GraphTextEndpoint {
                store: Arc::clone(store),
                text: Arc::clone(&text),
            },
        )
        .bind(
            UriTemplate::parse(format!("{TEXT_INDEX_PREFIX}{{graph}}"))
                .expect("the template is a literal with one trailing variable"),
            TextIndexEndpoint {
                text: Arc::clone(&text),
            },
        )
        .bind(
            Exact::new("urn:iki:store:text-enrollment"),
            EnrollmentEndpoint { text },
        )
}

fn denied_read(graphs: &[&NamedNode], door: &str) -> Error {
    let names: Vec<String> = graphs.iter().map(|g| format!("<{}>", g.as_str())).collect();
    let tokens: Vec<String> = graphs
        .iter()
        .map(|g| format!("`{}`", cap_read_graph(g.as_str())))
        .collect();
    Error::Denied(
        format!(
            "searching {} through `{door}` needs {} {}, which this capability does not hold. \
             Every graph a search names must be granted: the search is refused rather than \
             answered over the graphs that are. A grant names exactly one graph; holding \
             `{CAP_READ}` does not imply it",
            names.join(", "),
            if tokens.len() == 1 {
                "the grant"
            } else {
                "the grants"
            },
            tokens.join(", "),
        )
        .into(),
    )
}

struct GraphTextEndpoint {
    store: Arc<DurableStore>,
    text: Arc<TextIndexes>,
}

#[async_trait]
impl Endpoint for GraphTextEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        self.store.serving()?;
        match inv.request.verb {
            Verb::Source => {
                let set = GraphSet::parse(inv.inline_str("graph")?)?;
                let missing: Vec<&NamedNode> = set
                    .graphs()
                    .iter()
                    .filter(|g| !inv.capability.allows(&cap_read_graph(g.as_str())))
                    .collect();
                if !missing.is_empty() {
                    return Err(denied_read(&missing, "urn:iki:store:graph-text"));
                }
                let mode = optional_inline_str(inv, "mode")?;
                let any = match mode {
                    None | Some("all") => false,
                    Some("any") => true,
                    Some(other) => {
                        return Err(Error::InvalidArgument {
                            name: "mode".to_string(),
                            detail: format!(
                                "`{other}` is not a mode: give `all` (every word, the \
                                 default) or `any` (at least one word)"
                            ),
                        })
                    }
                };
                let limit = match optional_inline_str(inv, "limit")? {
                    None => DEFAULT_LIMIT,
                    Some(raw) => match raw.trim().parse::<usize>() {
                        Ok(n) if (1..=MAX_LIMIT).contains(&n) => n,
                        _ => {
                            return Err(Error::InvalidArgument {
                                name: "limit".to_string(),
                                detail: format!(
                                    "`{raw}` is not a limit: give a whole number from 1 to \
                                     {MAX_LIMIT}"
                                ),
                            })
                        }
                    },
                };
                let query = TextQuery {
                    words: optional_inline_str(inv, "words")?.map(str::to_string),
                    any,
                    phrase: optional_inline_str(inv, "phrase")?.map(str::to_string),
                    predicates: optional_inline_str(inv, "predicate")?
                        .map(|p| p.split_whitespace().map(str::to_string).collect())
                        .unwrap_or_default(),
                    limit,
                };
                let hits = match self.text.search(set.graphs(), &query) {
                    Ok(hits) => hits,
                    Err(SearchError::NoIndex(graphs)) => {
                        let names: Vec<String> = graphs
                            .iter()
                            .map(|(g, why)| format!("<{g}> ({why})"))
                            .collect();
                        return Err(Error::Unavailable(format!(
                            "no index: search by scanning instead. These graphs have no \
                             ready text index: {}. Nothing was searched. A stale or building \
                             index becomes ready when its rebuild finishes",
                            names.join(", ")
                        )));
                    }
                    Err(SearchError::NoWords(why)) => {
                        return Err(Error::InvalidArgument {
                            name: "words".to_string(),
                            detail: format!(
                                "give `words` or `phrase` with at least one word: {why}. \
                                 Nothing was searched"
                            ),
                        })
                    }
                    Err(SearchError::Index(e)) => return Err(Error::Endpoint(e)),
                };
                let hits: Vec<serde_json::Value> = hits
                    .into_iter()
                    .map(|h| {
                        serde_json::json!({
                            "subject": h.subject,
                            "predicate": h.predicate,
                            "graph": h.graph,
                            "score": h.score,
                        })
                    })
                    .collect();
                let body = serde_json::json!({ "schema": 1, "hits": hits }).to_string();
                let rep = Representation::new(ReprType::new("application/json"), body.into_bytes());
                Ok(with_freshness(rep, &self.store, Some(set.graphs())))
            }
            other => Err(unsupported("store-graph-text", other)),
        }
    }

    fn name(&self) -> &str {
        "store-graph-text"
    }

    fn describe(&self) -> Description {
        Description::new("store-graph-text")
            .title("Ranked text search over named graphs")
            .summary(
                "Search the literals of enrolled predicates in the named graphs, by words or a \
                 phrase, ranked by relevance across all of them. The answer is JSON: \
                 `{\"schema\":1,\"hits\":[{\"subject\",\"predicate\",\"graph\",\"score\"}]}`, \
                 most relevant first, with no text: read the text through \
                 `urn:iki:store:graph-select`. Words are matched whole, lowercased, without \
                 stemming; substrings are not matched. Every graph named needs its read grant, \
                 and a request naming one without it is refused, never answered over the \
                 rest. A graph whose index is not enrolled, stale or still building is refused \
                 as unavailable, starting `no index`, so a caller can search by scanning \
                 instead.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .requires(CAP_READ_GRAPH)
            .input(
                ArgSpec::new("graph")
                    .summary(
                        "The named graphs to search, separated by whitespace. The caller must \
                         hold `urn:cap:store:read:graph:<IRI>` for every one.",
                    )
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("words")
                    .optional()
                    .summary("Words to find, in any order and any field.")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("mode")
                    .optional()
                    .summary("`all` (the default): every word; `any`: at least one.")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("phrase")
                    .optional()
                    .summary("Words that must appear in order and next to each other.")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("predicate")
                    .optional()
                    .summary("Only hits through these predicates, separated by whitespace.")
                    .class(XSD_STRING),
            )
            .input(
                ArgSpec::new("limit")
                    .optional()
                    .summary("The most hits answered: 1 to 1000, 50 by default.")
                    .class(XSD_STRING),
            )
            .output("application/json")
    }
}

struct TextIndexEndpoint {
    text: Arc<TextIndexes>,
}

#[async_trait]
impl Endpoint for TextIndexEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let raw = inv.bindings.get("graph").unwrap_or_default();
        let (encoded, action) = match raw.rsplit_once(':') {
            Some((g, a @ ("rebuild" | "compact"))) => (g, Some(a)),
            _ => (raw, None),
        };
        let graph = graph_of_thread_suffix(encoded).ok_or_else(|| Error::InvalidArgument {
            name: "graph".to_string(),
            detail: format!(
                "`{encoded}` is not a percent-encoded graph IRI: build this resource's name \
                 with the graph IRI encoded as `ikigai_store::graph_thread` encodes it"
            ),
        })?;
        match (inv.request.verb, action) {
            (Verb::Source, None) => {
                let scope = cap_read_graph(&graph);
                if !inv.capability.allows(&scope) {
                    return Err(Error::Denied(
                        format!(
                            "reading the text index of graph <{graph}> needs the grant \
                             `{scope}`, which this capability does not hold"
                        )
                        .into(),
                    ));
                }
                // Uncached: a rebuild changes the state without a write.
                self.text.status(&graph).map(plain).map_err(Error::Endpoint)
            }
            (Verb::Sink, Some(action)) => {
                let scope = cap_write_graph(&graph);
                if !inv.capability.allows(&scope) {
                    return Err(Error::Denied(
                        format!(
                            "the {action} of graph <{graph}>'s text index needs the grant \
                             `{scope}`, which this capability does not hold"
                        )
                        .into(),
                    ));
                }
                let node =
                    NamedNode::new(graph.as_str()).map_err(|e| Error::Endpoint(e.to_string()))?;
                let done = if action == "rebuild" {
                    self.text
                        .rebuild_now(&graph)
                        .map(|n| n.map(|n| format!("rebuilt <{graph}>: {n} documents\n")))
                } else {
                    self.text
                        .compact(&graph)
                        .map(|n| n.map(|n| format!("compacted <{graph}>: {n} segments merged\n")))
                }
                .map_err(Error::Endpoint)?;
                let Some(text) = done else {
                    return Err(Error::InvalidArgument {
                        name: "graph".to_string(),
                        detail: format!(
                            "graph <{graph}> is not enrolled for text indexing, so it has no \
                             index to {action}. The host enrolls graphs; see \
                             `urn:iki:store:text-enrollment`"
                        ),
                    });
                };
                // Ranked scores may have changed: recompute cached searches of the graph.
                announce_graph_write(inv, &node).await?;
                Ok(plain(text))
            }
            (verb, _) => Err(unsupported("store-text-index", verb)),
        }
    }

    fn name(&self) -> &str {
        "store-text-index"
    }

    fn describe(&self) -> Description {
        Description::new("store-text-index")
            .title("One named graph's text index")
            .summary(
                "Source: the state of one graph's text index, as `label: value` lines \
                 (`state: ready|stale|building|not enrolled`, `format`, `fingerprint`, \
                 `documents`, `pending`, and `size` on disk), under the graph's read grant. \
                 Sink on `…:rebuild`: rebuild it from the store's quads, answering when done. \
                 Sink on `…:compact`: commit, merge away every deleted document and remove the \
                 old files before answering, so text a purge removed is gone from disk. Both \
                 Sinks need the graph's write grant.",
            )
            .verb(Verb::Source)
            .verb(Verb::Sink)
            .verb(Verb::Meta)
            .requires(CAP_READ_GRAPH)
            .input(
                ArgSpec::new("graph")
                    .binding()
                    .summary(
                        "The graph IRI, percent-encoded as `graph_thread` encodes it, then \
                         optionally `:rebuild` or `:compact`.",
                    )
                    .class(XSD_STRING),
            )
            .output("text/plain")
    }
}

struct EnrollmentEndpoint {
    text: Arc<TextIndexes>,
}

#[async_trait]
impl Endpoint for EnrollmentEndpoint {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        match inv.request.verb {
            Verb::Source => Ok(plain(self.text.enrollment().describe_lines())),
            other => Err(unsupported("store-text-enrollment", other)),
        }
    }

    fn name(&self) -> &str {
        "store-text-enrollment"
    }

    fn describe(&self) -> Description {
        Description::new("store-text-enrollment")
            .title("Which graphs and predicates are text-indexed")
            .summary(
                "The host's text-index enrollment, as `label: value` lines: each enrolled \
                 `graph`, each `graphs starting with` pattern, each `predicate` with its \
                 weight, and the `analyzer`.",
            )
            .verb(Verb::Source)
            .verb(Verb::Meta)
            .requires(CAP_READ_ANY)
            .output("text/plain")
    }
}
