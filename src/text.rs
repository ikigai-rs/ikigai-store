//! A full-text index over the graphs and predicates the host enrolls (ledger #4128), behind
//! the `text-index` feature. The design is ikigai-ledger `docs/design/text-index.md`.
//!
//! - **One index per enrolled named graph**, never one shared index with a graph field: BM25
//!   statistics of a shared index would let a reader of one graph watch another fill up.
//! - **A document is one (subject, predicate, literal)** of an enrolled graph under an enrolled
//!   predicate, keyed by a hash of the three. Words only, with positions, so phrases work. No
//!   text is stored: a hit names the subject, the predicate, the graph and a score, and the
//!   text itself is read from the store under the caller's grant.
//! - **Kept current from the scoped write door's own difference**: `graph-update` hands every
//!   quad it removed and added to [`TextIndexes::apply`], under the store's write lock. A broad
//!   update or load computes no difference, so it marks every enrolled graph stale and rebuilds
//!   it in the background ([`TextIndexes::all_stale`]).
//! - **Derived, never the only copy of anything.** On open, a missing index, another format, a
//!   different enrollment or a missing clean-close marker marks the graph stale and rebuilds it.
//!   A stale or building graph answers "no index", so a caller falls back to the scan.
//! - **Commits on a one-second timer**, and before any search that finds a write since the
//!   last commit, so an answer never misses a committed write.
//!
//! ⚠ Nothing tantivy-specific leaks into the names or the faces: another index (files,
//! documents, vectors) can serve the same contract.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use oxigraph::model::{NamedNode, NamedOrBlankNode, Quad, Term};
use oxigraph::store::Store;
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{Bm25StatisticsProvider, BooleanQuery, Occur, PhraseQuery, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value, STORED, STRING, TEXT};
use tantivy::tokenizer::TokenStream;
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term as IxTerm,
};

/// The on-disk format this crate writes. A different number on open means rebuild.
pub const TEXT_INDEX_FORMAT: u32 = 1;

/// The analyzer, named in the fingerprint and in `urn:iki:store:text-enrollment`.
const ANALYZER: &str = "words: split on non-alphanumerics, lowercased, no stemming";

/// How long written documents may wait before a commit makes them searchable.
pub const COMMIT_INTERVAL: Duration = Duration::from_secs(1);

/// The smallest writer budget tantivy accepts, per graph.
const WRITER_BYTES: usize = 15_000_000;

/// The meta file beside each graph's index.
const META: &str = "ikigai-text-index.json";

/// Which graphs and predicates are indexed: the host's configuration, never a caller's.
///
/// ★ Nothing is enrolled by default. A prefix pattern never enrolls a graph whose IRI ends in
/// `:deleted` (a graveyard) or `:session` (session summaries, which have their own grant): such
/// a graph is indexed only if named exactly.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextEnrollment {
    exact: Vec<String>,
    prefixes: Vec<String>,
    predicates: BTreeMap<String, f32>,
}

impl TextEnrollment {
    /// Nothing enrolled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enroll one named graph.
    pub fn graph(mut self, iri: impl Into<String>) -> Self {
        self.exact.push(iri.into());
        self
    }

    /// Enroll every named graph whose IRI starts with `prefix`, except graveyards and
    /// session-summary graphs (see the type docs).
    pub fn graphs_starting_with(mut self, prefix: impl Into<String>) -> Self {
        self.prefixes.push(prefix.into());
        self
    }

    /// Index this predicate's literals, at weight 1.
    pub fn predicate(self, iri: impl Into<String>) -> Self {
        self.predicate_weighted(iri, 1.0)
    }

    /// Index this predicate's literals; a hit through it scores `weight` times its BM25 score
    /// (the ledger weighs a title match 2x).
    pub fn predicate_weighted(mut self, iri: impl Into<String>, weight: f32) -> Self {
        self.predicates.insert(iri.into(), weight);
        self
    }

    /// Whether `graph` is enrolled.
    pub fn enrolls(&self, graph: &str) -> bool {
        if self.exact.iter().any(|g| g == graph) {
            return true;
        }
        let reserved = graph.ends_with(":deleted") || graph.ends_with(":session");
        !reserved && self.prefixes.iter().any(|p| graph.starts_with(p.as_str()))
    }

    fn weight(&self, predicate: &str) -> Option<f32> {
        self.predicates.get(predicate).copied()
    }

    /// What an index built under this enrollment depends on: a change means rebuild.
    fn fingerprint(&self) -> String {
        let preds: Vec<String> = self.predicates.keys().map(|p| format!("<{p}>")).collect();
        format!("v{TEXT_INDEX_FORMAT}; {ANALYZER}; {}", preds.join(" "))
    }

    /// The text of `urn:iki:store:text-enrollment`.
    pub(crate) fn describe_lines(&self) -> String {
        let mut out = String::new();
        for g in &self.exact {
            out.push_str(&format!("graph: <{g}>\n"));
        }
        for p in &self.prefixes {
            out.push_str(&format!(
                "graphs starting with: {p} (except those ending in :deleted or :session)\n"
            ));
        }
        for (p, w) in &self.predicates {
            out.push_str(&format!("predicate: <{p}> weight {w}\n"));
        }
        out.push_str(&format!("analyzer: {ANALYZER}\n"));
        if self.exact.is_empty() && self.prefixes.is_empty() {
            out.push_str("enrolled graphs: none\n");
        }
        out
    }
}

/// One hit: index-neutral, so any index can serve the same contract.
#[derive(Clone, Debug, PartialEq)]
pub struct TextHit {
    /// The subject's IRI, or `_:id` for a blank node.
    pub subject: String,
    /// The predicate's IRI.
    pub predicate: String,
    /// The graph's IRI.
    pub graph: String,
    /// Higher is more relevant; comparable within one answer only.
    pub score: f32,
}

/// What a search asks.
#[derive(Clone, Debug, Default)]
pub struct TextQuery {
    /// Words, any order; every one must occur unless `any`.
    pub words: Option<String>,
    /// Match a document with any of `words` rather than all of them.
    pub any: bool,
    /// Words that must occur in order and next to each other, in one literal.
    pub phrase: Option<String>,
    /// Only these predicates, when non-empty.
    pub predicates: Vec<String>,
    /// The most hits answered.
    pub limit: usize,
}

/// Why a search could not be answered by an index.
#[derive(Debug)]
pub enum SearchError {
    /// These graphs have no ready index (not enrolled, stale, or building): use the scan.
    NoIndex(Vec<(String, &'static str)>),
    /// The query has no word to look for.
    NoWords(&'static str),
    /// The index failed.
    Index(String),
}

/// The state of one graph's index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexState {
    /// Answers searches.
    Ready,
    /// Waiting for a rebuild; searches answer "no index".
    Stale,
    /// Being rebuilt; searches answer "no index".
    Building,
}

impl IndexState {
    fn word(self) -> &'static str {
        match self {
            IndexState::Ready => "ready",
            IndexState::Stale => "stale",
            IndexState::Building => "building",
        }
    }
}

#[derive(Clone, Copy)]
struct Fields {
    key: Field,
    subject: Field,
    predicate: Field,
    word: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let key = b.add_text_field("key", STRING);
    let subject = b.add_text_field("subject", STRING | STORED);
    let predicate = b.add_text_field("predicate", STRING | STORED);
    let word = b.add_text_field("word", TEXT);
    (
        b.build(),
        Fields {
            key,
            subject,
            predicate,
            word,
        },
    )
}

struct GraphIndex {
    graph: String,
    index: Index,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    state: Mutex<IndexState>,
    /// Documents added or deleted since the last commit.
    pending: AtomicUsize,
    /// Bumped by every change, so a rebuild that raced a write knows to run again.
    generation: AtomicU64,
    dir: Option<PathBuf>,
}

/// Every enrolled graph's index for one store.
pub struct TextIndexes {
    enrollment: TextEnrollment,
    root: Option<PathBuf>,
    store: Arc<Store>,
    fields: Fields,
    schema: Schema,
    graphs: Mutex<HashMap<String, Arc<GraphIndex>>>,
    me: Weak<TextIndexes>,
}

impl std::fmt::Debug for TextIndexes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextIndexes")
            .field("enrollment", &self.enrollment)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

fn ix(e: impl std::fmt::Display) -> String {
    format!("text index: {e}")
}

/// A stable 128-bit FNV-1a, so a key survives a toolchain upgrade.
fn key_of(subject: &str, predicate: &str, literal: &str) -> String {
    let mut h: u128 = 0x6c62272e07bb014262b821756295c58d;
    for part in [subject, "\u{0}", predicate, "\u{0}", literal] {
        for b in part.as_bytes() {
            h ^= u128::from(*b);
            h = h.wrapping_mul(0x0000000001000000000000000000013B);
        }
    }
    format!("{h:032x}")
}

fn subject_text(s: &NamedOrBlankNode) -> String {
    match s {
        NamedOrBlankNode::NamedNode(n) => n.as_str().to_string(),
        NamedOrBlankNode::BlankNode(b) => format!("_:{}", b.as_str()),
    }
}

impl TextIndexes {
    /// Open the indexes of `enrollment` over `store`: on disk under `root`, or in memory.
    /// Every enrolled graph the store already holds is opened now, and rebuilt in the
    /// background unless its index is current and was closed cleanly.
    pub(crate) fn open(
        store: Arc<Store>,
        root: Option<PathBuf>,
        enrollment: TextEnrollment,
    ) -> Result<Arc<Self>, String> {
        let (schema, fields) = schema();
        let this = Arc::new_cyclic(|me| TextIndexes {
            enrollment,
            root,
            store,
            fields,
            schema,
            graphs: Mutex::new(HashMap::new()),
            me: me.clone(),
        });
        for graph in this.enrolled_graphs_in_store()? {
            this.index_for(&graph)?;
        }
        spawn_commit_timer(Arc::downgrade(&this));
        Ok(this)
    }

    /// The host's enrollment.
    pub(crate) fn enrollment(&self) -> &TextEnrollment {
        &self.enrollment
    }

    fn enrolled_graphs_in_store(&self) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for g in self.store.named_graphs() {
            if let NamedOrBlankNode::NamedNode(n) = g.map_err(ix)? {
                if self.enrollment.enrolls(n.as_str()) {
                    out.push(n.as_str().to_string());
                }
            }
        }
        Ok(out)
    }

    fn dir_for(&self, graph: &str) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|r| r.join(format!("g{}", key_of(graph, "", ""))))
    }

    /// The graph's index, opened (and if need be rebuilt) on first use; `None` if not enrolled.
    fn index_for(&self, graph: &str) -> Result<Option<Arc<GraphIndex>>, String> {
        if !self.enrollment.enrolls(graph) {
            return Ok(None);
        }
        let mut graphs = self.graphs.lock().expect("text index map");
        if let Some(g) = graphs.get(graph) {
            return Ok(Some(Arc::clone(g)));
        }
        let dir = self.dir_for(graph);
        let (index, current) = match &dir {
            None => (Index::create_in_ram(self.schema.clone()), false),
            Some(dir) => self.open_dir(dir)?,
        };
        let writer = index.writer_with_num_threads(1, WRITER_BYTES).map_err(ix)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(ix)?;
        let g = Arc::new(GraphIndex {
            graph: graph.to_string(),
            index,
            writer: Mutex::new(writer),
            reader,
            state: Mutex::new(if current {
                IndexState::Ready
            } else {
                IndexState::Stale
            }),
            pending: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            dir,
        });
        graphs.insert(graph.to_string(), Arc::clone(&g));
        drop(graphs);
        self.write_meta(&g, false);
        if !current {
            self.spawn_rebuild(Arc::clone(&g));
        }
        Ok(Some(g))
    }

    /// Open `dir` if its meta says it is current and was closed cleanly; otherwise empty it.
    #[allow(clippy::disallowed_methods)] // ambient: fs: only the text-index subdirectory of the store's own directory, which the host passed to DurableStore::open
    fn open_dir(&self, dir: &PathBuf) -> Result<(Index, bool), String> {
        let meta = std::fs::read_to_string(dir.join(META)).ok();
        let current = meta
            .as_deref()
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .is_some_and(|m| {
                m["format"] == TEXT_INDEX_FORMAT
                    && m["fingerprint"] == self.enrollment.fingerprint()
                    && m["clean"] == true
            });
        if !current && dir.exists() {
            std::fs::remove_dir_all(dir).map_err(ix)?;
        }
        std::fs::create_dir_all(dir).map_err(ix)?;
        let mmap = MmapDirectory::open(dir).map_err(ix)?;
        match Index::open_or_create(mmap, self.schema.clone()) {
            Ok(index) => Ok((index, current)),
            Err(_) if current => {
                std::fs::remove_dir_all(dir).map_err(ix)?;
                self.open_dir(dir).map(|(i, _)| (i, false))
            }
            Err(e) => Err(ix(e)),
        }
    }

    #[allow(clippy::disallowed_methods)] // ambient: fs: only the text-index subdirectory of the store's own directory
    fn write_meta(&self, g: &GraphIndex, clean: bool) {
        if let Some(dir) = &g.dir {
            let meta = serde_json::json!({
                "format": TEXT_INDEX_FORMAT,
                "fingerprint": self.enrollment.fingerprint(),
                "graph": g.graph,
                "clean": clean,
            });
            let _ = std::fs::write(dir.join(META), meta.to_string());
        }
    }

    fn spawn_rebuild(&self, g: Arc<GraphIndex>) {
        let Some(me) = self.me.upgrade() else { return };
        std::thread::Builder::new()
            .name("ikigai-store text rebuild".into())
            .spawn(move || {
                let _ = me.rebuild(&g);
            })
            .ok();
    }

    /// Rebuild one graph's index from the store's quads, now, until no write raced it.
    fn rebuild(&self, g: &GraphIndex) -> Result<usize, String> {
        loop {
            {
                let mut state = g.state.lock().expect("text index state");
                if *state == IndexState::Building {
                    return Ok(0);
                }
                *state = IndexState::Building;
            }
            let started = g.generation.load(Ordering::SeqCst);
            let result = self.fill(g);
            let mut state = g.state.lock().expect("text index state");
            match result {
                Ok(n) if g.generation.load(Ordering::SeqCst) == started => {
                    *state = IndexState::Ready;
                    return Ok(n);
                }
                Ok(_) => *state = IndexState::Stale,
                Err(e) => {
                    *state = IndexState::Stale;
                    return Err(e);
                }
            }
        }
    }

    fn fill(&self, g: &GraphIndex) -> Result<usize, String> {
        let graph = NamedNode::new(g.graph.as_str()).map_err(ix)?;
        let mut writer = g.writer.lock().expect("text index writer");
        writer.delete_all_documents().map_err(ix)?;
        let mut n = 0;
        for quad in self
            .store
            .quads_for_pattern(None, None, None, Some(graph.as_ref().into()))
        {
            let quad = quad.map_err(ix)?;
            if let Some(doc) = self.document(&quad) {
                writer.add_document(doc).map_err(ix)?;
                n += 1;
            }
        }
        writer.commit().map_err(ix)?;
        g.pending.store(0, Ordering::SeqCst);
        g.reader.reload().map_err(ix)?;
        Ok(n)
    }

    fn key(&self, quad: &Quad) -> Option<String> {
        let Term::Literal(lit) = &quad.object else {
            return None;
        };
        self.enrollment.weight(quad.predicate.as_str())?;
        Some(key_of(
            &subject_text(&quad.subject),
            quad.predicate.as_str(),
            lit.value(),
        ))
    }

    fn document(&self, quad: &Quad) -> Option<TantivyDocument> {
        let key = self.key(quad)?;
        let Term::Literal(lit) = &quad.object else {
            return None;
        };
        let mut doc = TantivyDocument::default();
        doc.add_text(self.fields.key, key);
        doc.add_text(self.fields.subject, subject_text(&quad.subject));
        doc.add_text(self.fields.predicate, quad.predicate.as_str());
        doc.add_text(self.fields.word, lit.value());
        Some(doc)
    }

    /// Apply the difference a scoped write is committing to `graph`. Called under the
    /// store's write lock, so two writes reach the index in the order they reach the store.
    pub(crate) fn apply(&self, graph: &str, removed: &[Quad], added: &[Quad]) {
        let Ok(Some(g)) = self.index_for(graph) else {
            return;
        };
        let mut changed = 0;
        {
            let writer = g.writer.lock().expect("text index writer");
            for quad in removed {
                if let Some(key) = self.key(quad) {
                    writer.delete_term(IxTerm::from_field_text(self.fields.key, &key));
                    changed += 1;
                }
            }
            for quad in added {
                if let (Some(key), Some(doc)) = (self.key(quad), self.document(quad)) {
                    // Re-adding a quad replaces its document under the same key.
                    writer.delete_term(IxTerm::from_field_text(self.fields.key, &key));
                    if writer.add_document(doc).is_err() {
                        self.mark_stale(&g);
                        return;
                    }
                    changed += 1;
                }
            }
        }
        if changed > 0 {
            g.generation.fetch_add(1, Ordering::SeqCst);
            g.pending.fetch_add(changed, Ordering::SeqCst);
        }
    }

    fn mark_stale(&self, g: &Arc<GraphIndex>) {
        g.generation.fetch_add(1, Ordering::SeqCst);
        let mut state = g.state.lock().expect("text index state");
        if *state == IndexState::Ready {
            *state = IndexState::Stale;
        }
        drop(state);
        self.spawn_rebuild(Arc::clone(g));
    }

    /// A broad update or load wrote something this store cannot name: every enrolled graph is
    /// stale until rebuilt, including graphs it may have created.
    pub(crate) fn all_stale(&self) {
        if let Ok(graphs) = self.enrolled_graphs_in_store() {
            for graph in graphs {
                let _ = self.index_for(&graph);
            }
        }
        let open: Vec<Arc<GraphIndex>> = self
            .graphs
            .lock()
            .expect("text index map")
            .values()
            .cloned()
            .collect();
        for g in open {
            self.mark_stale(&g);
        }
    }

    fn commit(&self, g: &GraphIndex) -> Result<(), String> {
        if g.pending.load(Ordering::SeqCst) == 0 {
            return Ok(());
        }
        let mut writer = g.writer.lock().expect("text index writer");
        let pending = g.pending.swap(0, Ordering::SeqCst);
        if let Err(e) = writer.commit() {
            g.pending.fetch_add(pending, Ordering::SeqCst);
            return Err(ix(e));
        }
        g.reader.reload().map_err(ix)
    }

    fn commit_all(&self) {
        let open: Vec<Arc<GraphIndex>> = self
            .graphs
            .lock()
            .expect("text index map")
            .values()
            .cloned()
            .collect();
        for g in open {
            let _ = self.commit(&g);
        }
    }

    /// Rebuild `graph`'s index now. `None` if it is not enrolled.
    pub(crate) fn rebuild_now(&self, graph: &str) -> Result<Option<usize>, String> {
        let Some(g) = self.index_for(graph)? else {
            return Ok(None);
        };
        g.generation.fetch_add(1, Ordering::SeqCst);
        // Wait out a background rebuild already running, then run one of our own.
        loop {
            let state = *g.state.lock().expect("text index state");
            if state != IndexState::Building {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        *g.state.lock().expect("text index state") = IndexState::Stale;
        self.rebuild(&g).map(Some)
    }

    /// Commit, merge away every deleted document, and collect the old files, before
    /// answering: after this, deleted text is gone from the index's files. `None` if not
    /// enrolled. Answers the number of segments merged.
    pub(crate) fn compact(&self, graph: &str) -> Result<Option<usize>, String> {
        let Some(g) = self.index_for(graph)? else {
            return Ok(None);
        };
        g.pending.fetch_add(1, Ordering::SeqCst);
        self.commit(&g)?;
        let mut writer = g.writer.lock().expect("text index writer");
        let ids = g.index.searchable_segment_ids().map_err(ix)?;
        let merged = ids.len();
        if !ids.is_empty() {
            writer.merge(&ids).wait().map_err(ix)?;
        }
        writer.garbage_collect_files().wait().map_err(ix)?;
        drop(writer);
        g.reader.reload().map_err(ix)?;
        Ok(Some(merged))
    }

    /// The text of `urn:iki:store:text-index:graph:{graph}`.
    pub(crate) fn status(&self, graph: &str) -> Result<String, String> {
        let Some(g) = self.index_for(graph)? else {
            return Ok(format!("graph: <{graph}>\nstate: not enrolled\n"));
        };
        let state = *g.state.lock().expect("text index state");
        let searcher = g.reader.searcher();
        let mut text = format!(
            "graph: <{graph}>\nstate: {}\nformat: {TEXT_INDEX_FORMAT}\nfingerprint: {}\n\
             documents: {}\npending: {}\n",
            state.word(),
            self.enrollment.fingerprint(),
            searcher.num_docs(),
            g.pending.load(Ordering::SeqCst),
        );
        let deleted: u64 = searcher
            .segment_readers()
            .iter()
            .map(|r| u64::from(r.num_deleted_docs()))
            .sum();
        text.push_str(&format!(
            "deleted documents awaiting compaction: {deleted}\n"
        ));
        if let Some(dir) = &g.dir {
            text.push_str(&format!("size: {} bytes\n", dir_size(dir)));
        }
        Ok(text)
    }

    /// Search the named graphs' indexes as one: BM25 over their shared statistics, each
    /// graph's own index searched, nothing from any other graph counted.
    ///
    /// The caller has already checked the read grant of EVERY graph in `graphs`.
    pub(crate) fn search(
        &self,
        graphs: &[NamedNode],
        q: &TextQuery,
    ) -> Result<Vec<TextHit>, SearchError> {
        let mut ready = Vec::new();
        let mut missing = Vec::new();
        for graph in graphs {
            match self.index_for(graph.as_str()).map_err(SearchError::Index)? {
                None => missing.push((graph.as_str().to_string(), "not enrolled")),
                Some(g) => {
                    let state = *g.state.lock().expect("text index state");
                    if state == IndexState::Ready {
                        ready.push(g);
                    } else {
                        missing.push((graph.as_str().to_string(), state.word()));
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Err(SearchError::NoIndex(missing));
        }
        let query = self.query(&ready, q)?;
        for g in &ready {
            self.commit(g).map_err(SearchError::Index)?;
        }
        let searchers: Vec<Searcher> = ready.iter().map(|g| g.reader.searcher()).collect();
        let stats = UnionStatistics(&searchers);
        let mut hits = Vec::new();
        for (g, searcher) in ready.iter().zip(&searchers) {
            let all = usize::try_from(searcher.num_docs())
                .unwrap_or(usize::MAX)
                .max(1);
            let top = searcher
                .search_with_statistics_provider(
                    &*query,
                    &TopDocs::with_limit(all).order_by_score(),
                    &stats,
                )
                .map_err(|e| SearchError::Index(ix(e)))?;
            for (score, addr) in top {
                let doc: TantivyDocument =
                    searcher.doc(addr).map_err(|e| SearchError::Index(ix(e)))?;
                let get = |f| {
                    doc.get_first(f)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                let predicate = get(self.fields.predicate);
                if !q.predicates.is_empty() && !q.predicates.contains(&predicate) {
                    continue;
                }
                let weight = self.enrollment.weight(&predicate).unwrap_or(1.0);
                hits.push(TextHit {
                    subject: get(self.fields.subject),
                    predicate,
                    graph: g.graph.clone(),
                    score: score * weight,
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.graph.cmp(&b.graph))
                .then_with(|| a.subject.cmp(&b.subject))
                .then_with(|| a.predicate.cmp(&b.predicate))
        });
        hits.truncate(q.limit);
        Ok(hits)
    }

    fn tokens(&self, g: &GraphIndex, text: &str) -> Result<Vec<IxTerm>, SearchError> {
        let mut analyzer = g
            .index
            .tokenizer_for_field(self.fields.word)
            .map_err(|e| SearchError::Index(ix(e)))?;
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(IxTerm::from_field_text(
                self.fields.word,
                &stream.token().text,
            ));
        }
        Ok(out)
    }

    fn query(
        &self,
        ready: &[Arc<GraphIndex>],
        q: &TextQuery,
    ) -> Result<Box<dyn Query>, SearchError> {
        let Some(g) = ready.first() else {
            return Err(SearchError::NoWords("no graph was named"));
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        if let Some(words) = &q.words {
            let terms = self.tokens(g, words)?;
            if terms.is_empty() {
                return Err(SearchError::NoWords("`words` holds no word"));
            }
            let occur = if q.any { Occur::Should } else { Occur::Must };
            let inner: Vec<(Occur, Box<dyn Query>)> = terms
                .into_iter()
                .map(|t| {
                    (
                        occur,
                        Box::new(TermQuery::new(t, IndexRecordOption::WithFreqsAndPositions))
                            as Box<dyn Query>,
                    )
                })
                .collect();
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(inner))));
        }
        if let Some(phrase) = &q.phrase {
            let terms = self.tokens(g, phrase)?;
            match terms.len() {
                0 => return Err(SearchError::NoWords("`phrase` holds no word")),
                1 => clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(
                        terms.into_iter().next().expect("one term"),
                        IndexRecordOption::WithFreqsAndPositions,
                    )),
                )),
                _ => clauses.push((Occur::Must, Box::new(PhraseQuery::new(terms)))),
            }
        }
        if clauses.is_empty() {
            return Err(SearchError::NoWords(
                "neither `words` nor `phrase` was given",
            ));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }
}

impl Drop for TextIndexes {
    fn drop(&mut self) {
        let open: Vec<Arc<GraphIndex>> = self
            .graphs
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default();
        for g in open {
            let clean = *g.state.lock().expect("text index state") == IndexState::Ready
                && self.commit(&g).is_ok();
            self.write_meta(&g, clean);
        }
    }
}

/// Document counts and frequencies summed over exactly the graphs searched, so scores are
/// comparable across a union and say nothing about any other graph.
struct UnionStatistics<'a>(&'a [Searcher]);

impl Bm25StatisticsProvider for UnionStatistics<'_> {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        let mut n = 0;
        for s in self.0 {
            n += s.total_num_tokens(field)?;
        }
        Ok(n)
    }

    fn total_num_docs(&self) -> tantivy::Result<u64> {
        let mut n = 0;
        for s in self.0 {
            n += s.total_num_docs()?;
        }
        Ok(n)
    }

    fn doc_freq(&self, term: &IxTerm) -> tantivy::Result<u64> {
        let mut n = 0;
        for s in self.0 {
            n += Bm25StatisticsProvider::doc_freq(s, term)?;
        }
        Ok(n)
    }
}

fn spawn_commit_timer(indexes: Weak<TextIndexes>) {
    std::thread::Builder::new()
        .name("ikigai-store text commit".into())
        .spawn(move || loop {
            std::thread::sleep(COMMIT_INTERVAL);
            match indexes.upgrade() {
                Some(i) => i.commit_all(),
                None => return,
            }
        })
        .ok();
}

#[allow(clippy::disallowed_methods)] // ambient: fs: sizes the text-index subdirectory of the store's own directory
fn dir_size(dir: &PathBuf) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok()?.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}
