//! A named graph's history, kept so a scoped read can answer `as-of=T` (ledger #3731).
//!
//! The design is `docs/design/as-of-reads.md`; this is its first cut, and the short form is:
//!
//! - **An append-only log per named graph**, in memory, shared by every clone of a
//!   [`DurableStore`](crate::DurableStore). `urn:iki:store:graph-update` is the only door that
//!   knows exactly what it changed in one graph, so it is the only door that appends: the
//!   first write it records takes a **base** (the graph's whole contents after that write),
//!   and every later one a **diff** (the quads it added and removed).
//! - **A broad write breaks every log.** `urn:iki:store:update` and `urn:iki:store:load` can
//!   write any graph and do not say which, so each one appends a **gap** to every log. A
//!   fold that crosses a gap knows nothing, and is refused, until the next scoped write to
//!   that graph takes a fresh base. Answering across a gap would be a wrong answer that
//!   looked right.
//! - **Valid time is the time the store recorded the write**, in milliseconds since the Unix
//!   epoch. With no write able to backdate itself, "what was true at T" and "what the store
//!   held at T" are the same answer; knowledge time is the second step, in the design note.
//! - **T must be in the past.** A read at a time no write can still land at is a pure
//!   function of the log, so it is cacheable forever; a read at "now" is not, and is refused
//!   rather than answered live under a promise it cannot keep.

use oxigraph::model::{GraphName, NamedNode, Quad};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// One entry in a graph's log, at the millisecond it was recorded.
#[derive(Debug, Clone)]
enum Entry {
    /// The graph's whole contents at `at`: the fold starts again from here.
    Base { at: u64, quads: HashSet<Quad> },
    /// What one scoped write changed.
    Diff {
        at: u64,
        added: Vec<Quad>,
        removed: Vec<Quad>,
    },
    /// A write the log could not see (a broad update or a load): nothing is known after it
    /// until the next base.
    Gap { at: u64 },
}

impl Entry {
    fn at(&self) -> u64 {
        match self {
            Entry::Base { at, .. } | Entry::Diff { at, .. } | Entry::Gap { at } => *at,
        }
    }
}

/// Why a graph's history cannot answer as of a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unanswerable {
    /// The time is now or later, so a write could still land at it.
    NotPast { now: u64 },
    /// No base at or before the time, or a gap between the last base and it.
    NotKept,
}

#[derive(Debug, Default)]
struct Logs {
    graphs: HashMap<String, Vec<Entry>>,
    /// The latest time handed out, so two writes in one millisecond, or a clock stepped
    /// back, never order a later write before an earlier one.
    last: u64,
}

/// Every named graph's log. See the module docs.
#[derive(Debug, Default)]
pub(crate) struct History {
    logs: Mutex<Logs>,
}

fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl History {
    fn lock(&self) -> std::sync::MutexGuard<'_, Logs> {
        // A panic while holding the lock leaves a log whose last entry may be missing; the
        // entries it does hold are each whole, so the data is still sound to read.
        self.logs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn stamp(logs: &mut Logs) -> u64 {
        let at = wall_clock_ms().max(logs.last);
        logs.last = at;
        at
    }

    /// Record one scoped write to `graph`. `contents` is called only when the log needs a
    /// base (none yet, or a gap since the last one), and answers the graph's contents
    /// after the write. Called under the store's write lock, after the commit.
    pub(crate) fn record<F>(
        &self,
        graph: &NamedNode,
        added: Vec<Quad>,
        removed: Vec<Quad>,
        contents: F,
    ) where
        F: FnOnce() -> Option<HashSet<Quad>>,
    {
        let mut logs = self.lock();
        let at = Self::stamp(&mut logs);
        let log = logs.graphs.entry(graph.as_str().to_string()).or_default();
        let needs_base = matches!(log.last(), None | Some(Entry::Gap { .. }));
        if needs_base {
            match contents() {
                Some(quads) => log.push(Entry::Base { at, quads }),
                // Could not read the graph back: the log knows nothing from here.
                None => log.push(Entry::Gap { at }),
            }
        } else {
            log.push(Entry::Diff { at, added, removed });
        }
    }

    /// Record a write that may have changed any graph without saying which.
    pub(crate) fn break_all(&self) {
        let mut logs = self.lock();
        let at = Self::stamp(&mut logs);
        for log in logs.graphs.values_mut() {
            if !matches!(log.last(), None | Some(Entry::Gap { .. })) {
                log.push(Entry::Gap { at });
            }
        }
    }

    /// `graph`'s quads as of `at`, folded from its log.
    pub(crate) fn as_of(&self, graph: &str, at: u64) -> Result<HashSet<Quad>, Unanswerable> {
        let logs = self.lock();
        let now = wall_clock_ms().max(logs.last.saturating_add(1));
        if at >= now {
            return Err(Unanswerable::NotPast { now });
        }
        let log = logs.graphs.get(graph).ok_or(Unanswerable::NotKept)?;
        let mut state: Option<HashSet<Quad>> = None;
        for entry in log.iter().take_while(|e| e.at() <= at) {
            match entry {
                Entry::Base { quads, .. } => state = Some(quads.clone()),
                Entry::Diff { added, removed, .. } => {
                    if let Some(state) = state.as_mut() {
                        for quad in removed {
                            state.remove(quad);
                        }
                        state.extend(added.iter().cloned());
                    }
                }
                Entry::Gap { .. } => state = None,
            }
        }
        state.ok_or(Unanswerable::NotKept)
    }

    /// Forget `graph`'s whole history. As-of reads of it are refused until a scoped write
    /// takes a new base.
    pub(crate) fn purge(&self, graph: &str) -> bool {
        self.lock().graphs.remove(graph).is_some()
    }

    /// Fold everything recorded for `graph` up to `before` into one base at `before`, and
    /// drop the entries it replaced: reads as of `before` or later answer as they did, and
    /// reads earlier than `before` are refused.
    pub(crate) fn compact(&self, graph: &str, before: u64) {
        let mut logs = self.lock();
        let Some(log) = logs.graphs.get_mut(graph) else {
            return;
        };
        let split = log.iter().take_while(|e| e.at() <= before).count();
        if split == 0 {
            return;
        }
        let mut state: Option<HashSet<Quad>> = None;
        for entry in log.drain(..split) {
            match entry {
                Entry::Base { quads, .. } => state = Some(quads),
                Entry::Diff { added, removed, .. } => {
                    if let Some(state) = state.as_mut() {
                        for quad in &removed {
                            state.remove(quad);
                        }
                        state.extend(added);
                    }
                }
                Entry::Gap { .. } => state = None,
            }
        }
        let head = match state {
            Some(quads) => Entry::Base { at: before, quads },
            None => Entry::Gap { at: before },
        };
        log.insert(0, head);
    }

    /// How many entries `graph`'s log holds: its retention cost, in writes.
    pub(crate) fn entries(&self, graph: &str) -> usize {
        self.lock().graphs.get(graph).map_or(0, Vec::len)
    }
}

/// Quads of `graph` from an iterator over a store, as a set: a base.
pub(crate) fn graph_contents(
    store: &oxigraph::store::Store,
    graph: &NamedNode,
) -> Option<HashSet<Quad>> {
    let name = GraphName::from(graph.clone());
    store
        .quads_for_pattern(None, None, None, Some(name.as_ref()))
        .collect::<Result<HashSet<Quad>, _>>()
        .ok()
}
