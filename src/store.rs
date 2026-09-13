//! The durable dataset: who opens it, who may write to it, and what the handle
//! leaving this crate costs.
//!
//! Read `docs/design/handle-model.md` before changing anything here. The shape of
//! this module is a decision made from measurement, not a convenience.

// `Path` is named only by the durable constructors; `PathBuf` by `Backing`, always.
#[cfg(all(feature = "persistent", not(target_family = "wasm")))]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use ikigai_core::{Error, Result};

/// The canonical store type, re-exported so a host names ONE `Store`.
///
/// Rust unifies `oxigraph::store::Store` across crates only when every crate in the
/// build resolves the same `oxigraph`. Depending on `ikigai_store::Store` rather than
/// adding a direct `oxigraph` dependency makes that alignment structural instead of
/// coincidental — the same reason `ikigai-sparql` re-exports it.
pub use oxigraph::store::Store;

/// Where a [`DurableStore`]'s bytes live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backing {
    /// In memory: gone at process exit. The default, and the only backing that
    /// exists without the `persistent` feature or on wasm.
    Memory,
    /// A RocksDB directory this process holds the write lock on.
    Durable(PathBuf),
}

impl std::fmt::Display for Backing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backing::Memory => write!(f, "memory"),
            Backing::Durable(path) => write!(f, "durable {}", path.display()),
        }
    }
}

/// An RDF dataset this crate owns the bytes of.
///
/// # The central API decision: `open` versus `open_shared`
///
/// The kernel can invalidate a cached read only when it can SEE every write. A raw
/// `Arc<Store>` in someone else's hands is a writer the kernel cannot see, so handing
/// one out forfeits the golden-thread coverage this crate exists to provide
/// (`ikigai-sparql`'s `UPDATE_THREAD` docs work the argument through).
///
/// Prose cannot hold that line — the handout would be one accessor call in a host,
/// months later, with the caching still declared. So the two modes are two
/// constructors, and **there is no accessor that turns the first into the second**:
///
/// - `open` (feature `persistent`) / [`in_memory`](DurableStore::in_memory) — *owned*.
///   Nothing else holds the handle and (durably) no other process can even open the
///   directory, so reads are `.cacheable()` under the write endpoints' threads.
/// - `open_shared` / [`in_memory_shared`](DurableStore::in_memory_shared) — the handle leaves at
///   construction, for a host that wants `ikigai_sparql::space_with_store` over the
///   same dataset. An invisible writer may exist for the whole life of the store, so
///   reads are `Expiry::Always`, permanently and by construction.
///
/// The choice is therefore made at the call site, visibly, on the line where the cost
/// is taken. [`is_covered`](DurableStore::is_covered) reports which it was.
#[derive(Clone)]
pub struct DurableStore {
    store: Arc<Store>,
    backing: Backing,
    covered: bool,
}

/// Hand-written because `oxigraph::store::Store` has no `Debug` — and because the two
/// fields that matter to a reader are the ones a derive could not have chosen to print.
impl std::fmt::Debug for DurableStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableStore")
            .field("backing", &self.backing)
            .field("covered", &self.covered)
            .finish_non_exhaustive()
    }
}

impl DurableStore {
    /// An empty in-memory dataset, owned. Nothing survives process exit.
    pub fn in_memory() -> Result<Self> {
        Ok(DurableStore {
            store: Arc::new(Store::new().map_err(endpoint_err)?),
            backing: Backing::Memory,
            covered: true,
        })
    }

    /// An in-memory dataset **and a clone of its handle**. See the type docs: taking
    /// the handle makes every read `Expiry::Always` for the life of this store.
    pub fn in_memory_shared() -> Result<(Self, Arc<Store>)> {
        let mut this = Self::in_memory()?;
        this.covered = false;
        let handle = Arc::clone(&this.store);
        Ok((this, handle))
    }

    /// Open the durable dataset at `path`, taking the write lock for the life of this
    /// process (model A — see `docs/design/handle-model.md`).
    ///
    /// The directory is created if it does not exist. **The refusal when it is already
    /// held is loud and legible**: [`Error::Unavailable`] naming the path and what to
    /// look for, rather than RocksDB's internal lock string surfacing raw. The error is
    /// typed `Unavailable` rather than `Endpoint` because it is exactly the transient,
    /// retryable shape the overlays gate on — the holder may exit.
    ///
    /// ⚠ **This constructor does not exist on wasm**, even with `persistent` enabled:
    /// RocksDB is native-linked and Oxigraph target-gates it out. A call is then a
    /// compile error naming the missing item — which is both louder and earlier than a
    /// runtime refusal, and leaves no wasm-only code path that no CI job can reach. (A
    /// store that quietly was not durable is the failure this crate is named after; a
    /// store whose *fallback* is only exercised on a target nothing lints is the next
    /// one.) The wasm face is [`in_memory`](DurableStore::in_memory), or a durable store
    /// reached over the wire.
    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(DurableStore {
            store: Arc::new(open_rocksdb(path.as_ref())?),
            backing: Backing::Durable(path.as_ref().to_path_buf()),
            covered: true,
        })
    }

    /// `open` **and a clone of the handle**. See the type docs:
    /// taking the handle makes every read `Expiry::Always` for the life of this store.
    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    pub fn open_shared(path: impl AsRef<Path>) -> Result<(Self, Arc<Store>)> {
        let mut this = Self::open(path)?;
        this.covered = false;
        let handle = Arc::clone(&this.store);
        Ok((this, handle))
    }

    /// Where this store's bytes live.
    pub fn backing(&self) -> &Backing {
        &self.backing
    }

    /// Whether the golden-thread coverage holds: `true` when the handle never left this
    /// crate, so every write is a write the kernel saw. `false` after
    /// `open_shared` — and then reads are never cacheable.
    pub fn is_covered(&self) -> bool {
        self.covered
    }

    /// The dataset, for this crate's endpoints. Deliberately **not** `pub`: see the
    /// type docs for why handing this out is a constructor-level decision.
    pub(crate) fn dataset(&self) -> &Store {
        &self.store
    }
}

/// Open (creating if needed) a RocksDB-backed store, mapping the one failure an
/// operator will actually hit into a sentence.
#[cfg(all(feature = "persistent", not(target_family = "wasm")))]
fn open_rocksdb(path: &Path) -> Result<Store> {
    // Fail loud on an unwritable path, and say which path. Without this the first
    // symptom is RocksDB's own errno string with no ikigai context around it.
    if let Err(e) = std::fs::create_dir_all(path) {
        return Err(Error::Endpoint(format!(
            "cannot create the store directory {}: {e}",
            path.display()
        )));
    }
    Store::open(path).map_err(|e| {
        let text = e.to_string();
        if is_lock_error(&text) {
            // ⚠ Matched on text because RocksDB reports the lock as a generic IO error
            // and oxigraph passes it through untyped.
            // `a_second_open_in_the_same_process_is_refused_and_the_refusal_is_legible`
            // in tests/handle_model.rs pins this against a real second open, so an
            // upstream rewording fails a test instead of silently degrading the message
            // to the raw string.
            Error::Unavailable(format!(
                "the store at {} is already open read-write and RocksDB permits one \
                 writer per directory. Another ikigai host (or another handle in this \
                 process) holds it; that process must exit, or this one must reach the \
                 data over the wire. Underlying error: {text}",
                path.display()
            ))
        } else {
            Error::Endpoint(format!("opening the store at {}: {text}", path.display()))
        }
    })
}

/// Whether a RocksDB error string is the one-writer-per-directory refusal.
///
/// Two spellings are known and both are matched: `Resource temporarily unavailable`
/// (a second PROCESS, a POSIX advisory lock conflict) and `lock hold by current
/// process` / `No locks available` (a second handle in the SAME process, caught by
/// RocksDB's own in-process registry — measured 2026-09-13, see the design doc).
#[cfg(all(feature = "persistent", not(target_family = "wasm")))]
fn is_lock_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("lock") && (lower.contains("unavailable") || lower.contains("no locks"))
}

fn endpoint_err(e: impl std::fmt::Display) -> Error {
    Error::Endpoint(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owned_in_memory_store_is_covered() {
        let store = DurableStore::in_memory().unwrap();
        assert!(store.is_covered());
        assert_eq!(store.backing(), &Backing::Memory);
    }

    #[test]
    fn handing_out_the_handle_forfeits_coverage_at_construction() {
        let (store, handle) = DurableStore::in_memory_shared().unwrap();
        assert!(
            !store.is_covered(),
            "a store that handed out its handle must never claim coverage"
        );
        // And the handle really is the same dataset — that is what was paid for.
        handle
            .load_from_slice(
                oxigraph::io::RdfFormat::NTriples,
                b"<http://ex/a> <http://ex/p> <http://ex/b> .",
            )
            .unwrap();
        assert_eq!(store.dataset().len().unwrap(), 1);
    }

    #[cfg(all(feature = "persistent", not(target_family = "wasm")))]
    #[test]
    fn the_lock_matcher_knows_both_spellings() {
        assert!(is_lock_error(
            "IO error: While lock file: /tmp/x/LOCK: Resource temporarily unavailable"
        ));
        assert!(is_lock_error(
            "IO error: lock hold by current process, acquire time 1 acquiring thread 2: \
             /tmp/x/LOCK: No locks available"
        ));
        assert!(!is_lock_error("IO error: No space left on device"));
    }
}
