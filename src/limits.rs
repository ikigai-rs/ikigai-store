//! Bounds on caller-supplied SPARQL, checked before the parser sees a byte (ledger #915).
//!
//! oxigraph's SPARQL parser (`spargebra`, a `peg` grammar) and its evaluator (`sparopt`,
//! `spareval`) are **recursive**, so a query's shape is a claim on the stack of whatever
//! thread parses it. Running out is not an error a caller gets back: Rust aborts the
//! **whole process** on a stack overflow, on any thread. `SELECT * WHERE { FILTER(((…1…))) }`
//! with ~1000 parentheses took down a host through its HTTP door, and the anonymous grant
//! held the read authority that reached the parse.
//!
//! Two layers, because neither is enough alone:
//!
//! 1. **[`check_sparql`] refuses, never truncates.** A query or update over
//!    [`MAX_SPARQL_BYTES`], or that nests deeper than [`MAX_SPARQL_NESTING`] — brackets,
//!    and runs of the one prefix operator spargebra recurses on, `!` — or nests RDF 1.2
//!    triple terms deeper than [`MAX_RDF_NESTING`], is refused with a typed
//!    `InvalidArgument` naming the bound, before anything is parsed. Those are the
//!    cheapest ways to recurse (ONE byte a level; ~2.3 KiB of stack a level for a bracket
//!    in a release build), so this is the check that turns a 3 KB query that killed a host
//!    into a refusal.
//! 2. **[`on_sparql_stack`] runs the parse AND the evaluation on a thread whose stack is
//!    sized from the query's length.** Nesting is not the only recursion: measured on a
//!    2 MiB thread in a release build, 1,879 triple patterns, 2,087 `FILTER`s, a 2,086-way
//!    `||` or 3,283 `+` terms all overflow — and those are shapes a generated query can
//!    legitimately have, so no lexical bound can refuse them without refusing real work.
//!    What bounds them is the byte bound times the stack each byte can cost, and the thread
//!    is where that budget lives: [`STACK_PER_BYTE`] per byte above a [`STACK_BASE`] floor.
//!    A thread's stack is reserved address space, committed only as it is touched, so an
//!    ordinary query costs the spawn and nothing else.
//!
//! ★ **The thread is sized for the BUILD, so a debug and a release build admit and answer the
//! same queries** (ledger #1003). An unoptimized build spends up to ~70× the stack per level (a
//! 2 MiB thread holds 46 `+` terms when evaluating, against 3,282 in release), and through
//! 0.2.9 the release sizing applied to both: a debug host aborted on a `1*1*…` chain of ~405
//! terms, well inside [`crate::budget::MAX_ALGEBRA_NODES`]. [`sparql_stack_size`] adds, when
//! `debug_assertions` are on, [`DEBUG_STACK_PER_NODE`] for every node the algebra bound
//! admits and [`DEBUG_STACK_PER_BYTE`] in place of [`STACK_PER_BYTE`]; both are measured.
//! Sizing rather than a depth bound, because a bound low enough for a debug build would
//! refuse in release what release answers, and because an `IN` list — one algebra node
//! however long — recurses once a member, so no node count could bound it.
//! The bracket bound holds in both: a debug build at [`MAX_SPARQL_NESTING`] of the costliest
//! bracket (`STR(` calls, ~60 KiB a level) needs ~4 MiB, well inside [`STACK_BASE`].
//!
//! ⚠ **And on wasm there is no thread**: [`on_sparql_stack`] runs inline there, so only the
//! first layer applies, on whatever stack the host gave the module.
//!
//! ⚠ **Neither layer bounds TIME.** The evaluator is quadratic in an operator chain's
//! length: 40,000 `||1` terms (120 KB) take ~30 s of a core in a release build and 80,000
//! more than two minutes, and long property paths, long BGPs and deeply nested collections
//! are slow well before they are deep. This module is about the stack — an abort takes
//! down every request at once. Time is [`crate::budget`]'s (ledger #964): this crate's doors
//! run every evaluation within one, on a thread sized exactly as [`on_sparql_stack`] sizes
//! it. ★ A consumer evaluating SPARQL over a dataset of its OWN runs it through
//! [`crate::budget::EvaluationBudget::run`] instead of [`on_sparql_stack`]: the same stack, and
//! a time budget (ledger #1327).
//!
//! # The scan is exact about what it skips, and that took an automaton
//!
//! The obvious scan — count `(`, `{`, `[`, skipping strings, `<IRI>`s and `#` comments —
//! is **wrong in a way an attacker can use**, because SPARQL's lexing of `<` depends on
//! where the parser is: in a triple pattern `<…>` is an IRI, but in an expression `?a<…`
//! is a less-than. spargebra accepts `<` + anything-but-`>` + `>` as an IRI only where an
//! IRI is allowed and the text inside is a valid IRI; elsewhere it is an operator, and the
//! "IRI" is code. So `( ?a<'> ) ' && ( ?a<'> ) ' && (…` reads to a skip-the-IRI scan as one
//! level, while the parser sees `(?a < "> ) " && (?a < "> ) " && (…` — a string it opens
//! INSIDE what the scan skipped — and nests once per repetition. A `#` inside such a token
//! does the same with a comment.
//!
//! So the scan follows **every reading** the grammar allows, as a small set of branches, and
//! refuses on the deepest. At a `<` it forks: an IRI (when the text up to the next `>` could
//! be one), a `<<` or `<<(` triple-term opener (when one follows; `<<(` and its closer `)>>`
//! are single tokens, so that `(` opens no expression), and a less-than — the last only
//! when the innermost open bracket is `(`, because every SPARQL expression sits directly
//! inside parentheses, so that is the only place a less-than can occur. Branches in the
//! same lexical state with the same open brackets are merged, so an ordinary query runs as
//! one branch and the whole scan stays linear. A closer pops only the opener it matches, so
//! no reading can be made to look shallower than it is; and a query that keeps more than
//! [`MAX_SCAN_BRANCHES`] readings alive at once is refused as too ambiguous to bound.
//!
//! ⚠ One more reading lives OUTSIDE the grammar: spargebra's `standard-unicode-escaping`
//! feature rewrites `\uXXXX` and `\UXXXXXXXX` anywhere in the text **before** tokenizing,
//! so `\u0028` is a `(`. oxigraph does not enable it, but feature unification is global and
//! a sibling could; when the text contains such an escape, the scan also runs over the
//! unescaped text and refuses on either.
//!
//! # A loaded DOCUMENT is bounded too (ledger #1428)
//!
//! `urn:iki:store:load` parses RDF, not SPARQL, so none of the above reaches it; its depth bound
//! is [`crate::depth`]'s. Three more bounds, all checked before the store's writer is asked for
//! anything, so a refused document never waits behind another write or holds one up:
//!
//! - **Bytes.** A document over [`MAX_LOAD_BYTES`] (or the host's bound,
//!   [`DurableStore::with_max_load_bytes`](crate::DurableStore::with_max_load_bytes)) is
//!   refused, naming the bound.
//! - **No XML entities.** oxrdfxml resolves an `<!ENTITY>` value against the entities declared
//!   before it AT DECLARATION TIME and stores it expanded, so a chain of entities each naming
//!   the last a few times ("billion laughs") turns ~500 bytes into megabytes and ~700 into
//!   gigabytes. An RDF/XML document that declares any entity is refused; one with a DOCTYPE
//!   that declares none still loads.
//! - **What the statements spell out.** The same shape needs no entities: a Turtle or TriG
//!   `@prefix` or `@base`, an RDF/XML namespace or `xml:base`, and the door's own `graph`
//!   argument are each written once and copied into every statement that uses them, so a
//!   prefix IRI of 128 KB and a thousand three-name statements spell out ~400 MB of terms, and
//!   the copying alone holds a core for as long as the document is long. So the document is
//!   parsed once WITHOUT storing anything, counting the bytes of every term its statements
//!   produce (IRIs, literal values, non-plain datatypes, language tags, blank-node labels, the
//!   graph name), and refused past the same bound: the rule a decoder follows, to count what
//!   it PRODUCES, not what it reads (ledger #1331). N-Triples and N-Quads write every term out
//!   in full, so what they produce never exceeds what they carry; a restore of a whole dataset
//!   as N-Quads (gonk's, 84 MB on 2026-10-13) passes on the byte bound alone.
//!
//! ⚠ **The document is parsed twice.** The counting pass stores nothing, and the load parses
//! again on the writer to store it. Holding the parsed statements between the two instead
//! would keep a document's whole expansion in memory per concurrent caller, where the writer
//! used to parse one document at a time; a second parse of a document that passed costs a
//! fraction of the insert.

use crate::depth::MAX_RDF_NESTING;
use ikigai_core::{Error, Result};

/// The most bytes a SPARQL query or update may carry: **1 MiB**.
///
/// Generous on purpose. The largest legitimate queries in the ecosystem are FLAT — a
/// `VALUES` list of every open item in a ledger is ~51 bytes an item, so this holds about
/// 20,000 of them, and `VALUES` and `INSERT DATA` recurse not at all (measured to 40,000
/// entries on a 2 MiB thread). What this bounds is the stack [`on_sparql_stack`] may need.
pub const MAX_SPARQL_BYTES: usize = 1 << 20;

/// The most bytes a document `urn:iki:store:load` takes may hold, and the most bytes of
/// terms its statements may spell out once parsed, unless the host sets another bound
/// ([`DurableStore::with_max_load_bytes`](crate::DurableStore::with_max_load_bytes)):
/// **256 MiB** (ledger #1428).
pub const MAX_LOAD_BYTES: usize = 256 << 20;

/// The deepest a SPARQL query or update may nest `(`, `{` and `[` — with a run of `!`
/// counting one level for each — **64**. Triple terms (`<<( … )>>`, `<< … >>`) are counted
/// apart, against [`MAX_RDF_NESTING`] (ledger #1354).
///
/// The deepest legitimate query in the ecosystem's tests and stored queries nests about 5
/// (a scan of ~700 of them, 2026-10-09). On a 2 MiB thread oxigraph 0.5.11's parser
/// overflows at ~180 levels of `(`/`{` in a debug build and ~850 in release, and at 34
/// (debug) or 426 (release) levels of function calls, the costliest bracket per level. 64 is
/// an order of magnitude above real use, and the parse runs on [`on_sparql_stack`], where a
/// debug build at this bound needs ~4 MiB of the [`STACK_BASE`] it gets.
pub const MAX_SPARQL_NESTING: usize = 64;

/// How many simultaneous readings the scan follows before refusing the text as too
/// ambiguous to bound. An ordinary query keeps one; it takes a `<` inside parentheses
/// followed by text that could be an IRI containing `'`, `#` or a bracket to make two.
pub const MAX_SCAN_BRANCHES: usize = 64;

/// The stack every SPARQL thread gets before the per-byte share: 16 MiB.
pub const STACK_BASE: usize = 16 << 20;

/// The stack reserved per byte of query, above [`STACK_BASE`]: 512 bytes.
///
/// The worst cost measured per byte in a release build, for the shapes the nesting bound
/// does not refuse, is ~370 (a property path of one-letter prefixed names, `:a/:a/…`), then
/// ~335 (`1||1||…`) and ~320 (`1+1+…`); ~260 for a `UNION` chain. 512 covers the worst with
/// margin, so a query at [`MAX_SPARQL_BYTES`] gets a 528 MiB thread — reserved, and touched
/// only as deep as the query actually recurses. (A run of `!` would cost ~650 a byte at
/// scale, which is why it counts as nesting instead.) `tests/sparql_stack_measure.rs`
/// re-measures all of these.
pub const STACK_PER_BYTE: usize = 512;

/// In a build with `debug_assertions` ON, the stack reserved per byte of query in place of
/// [`STACK_PER_BYTE`]: **2 KiB** (ledger #1003).
///
/// Covers what an unoptimized build spends per byte BEFORE the algebra bound can refuse, or
/// where that bound does not reach: parsing an arithmetic chain `1*1*…` costs ~900 bytes of
/// stack a byte (the algebra bound is checked after the parse, so the thread must hold the
/// parse of any chain the byte bound admits), and evaluating `1 IN (1,1,…)` ~1,245 — `IN` is
/// one algebra node however long its list, and the evaluator rewrites it into one `||` level
/// a member. A query at [`MAX_SPARQL_BYTES`] gets a ~2 GiB thread in a debug build: reserved
/// address space, touched only as deep as the query recurses.
pub const DEBUG_STACK_PER_BYTE: usize = 2 << 10;

/// In a build with `debug_assertions` ON, the stack reserved for each node the algebra bound
/// admits ([`crate::budget::MAX_ALGEBRA_NODES`]), on top of [`STACK_BASE`]: **64 KiB**, so
/// 64 MiB in all (ledger #1003).
///
/// The evaluator recurses once per algebra node, and an unoptimized build spends up to
/// ~45 KiB a level on it: `+` and `*` chains; then ~21 KiB for `OPTIONAL`, `BIND` and a path
/// step, ~13 KiB for `UNION`, ~7 KiB for `||` and `&&` (measured on oxigraph 0.5.11 by
/// `tests/sparql_stack_measure.rs`). A release build spends ~640 bytes a node at worst, so
/// the whole bound fits in [`STACK_BASE`] there and this applies to debug builds only.
pub const DEBUG_STACK_PER_NODE: usize = 64 << 10;

/// The stack a thread that parses and evaluates `text` is given: [`STACK_BASE`] plus
/// [`STACK_PER_BYTE`] a byte in a release build; in a build with `debug_assertions` on,
/// [`STACK_BASE`] plus [`DEBUG_STACK_PER_NODE`] for every node of
/// [`crate::budget::MAX_ALGEBRA_NODES`] plus [`DEBUG_STACK_PER_BYTE`] a byte.
///
/// `debug_assertions` is the compile-time signal that stands in for "oxigraph is
/// unoptimized". It errs safe in both mixed profiles a host is likely to use — optimized
/// dependencies under a dev profile, or debug assertions in release, each reserve more than
/// needed; a release profile that turns optimization OFF without debug assertions is the
/// one it would undersize.
///
/// ```
/// use ikigai_store::limits::{sparql_stack_size, STACK_BASE, STACK_PER_BYTE};
///
/// let query = "SELECT * WHERE { ?s ?p ?o }";
/// if cfg!(debug_assertions) {
///     assert!(sparql_stack_size(query) > STACK_BASE + 64 * 1024 * 1024);
/// } else {
///     assert_eq!(sparql_stack_size(query), STACK_BASE + query.len() * STACK_PER_BYTE);
/// }
/// ```
pub fn sparql_stack_size(text: &str) -> usize {
    sparql_stack_size_for_len(text.len())
}

/// [`sparql_stack_size`] for a text of `len` bytes.
pub(crate) fn sparql_stack_size_for_len(len: usize) -> usize {
    if cfg!(debug_assertions) {
        STACK_BASE
            .saturating_add(crate::budget::MAX_ALGEBRA_NODES.saturating_mul(DEBUG_STACK_PER_NODE))
            .saturating_add(len.saturating_mul(DEBUG_STACK_PER_BYTE))
    } else {
        STACK_BASE.saturating_add(len.saturating_mul(STACK_PER_BYTE))
    }
}

/// Refuse a SPARQL query or update that exceeds [`MAX_SPARQL_BYTES`], nests its brackets
/// deeper than [`MAX_SPARQL_NESTING`], or nests RDF 1.2 triple terms deeper than
/// [`MAX_RDF_NESTING`], naming the argument `arg` and the bound. Never truncates.
///
/// The two depths are counted apart (ledger #1354): `(`, `{`, `[` and a run of `!` against
/// [`MAX_SPARQL_NESTING`]; `<<( … )>>` and `<< … >>` against [`MAX_RDF_NESTING`], the depth
/// a loaded document may nest triple terms, so an update may write any term a load may. A
/// text at both bounds at once recurses 128 levels in the parser, which the thread
/// [`on_sparql_stack`] gives it holds many times over (`tests/sparql_stack_measure.rs`).
///
/// Every door in this crate that parses caller text calls this first. It is public so a
/// host with its own SPARQL face over oxigraph can apply the same bound; that host must
/// still parse on a big enough stack (see [`on_sparql_stack`]).
///
/// ```
/// use ikigai_store::depth::MAX_RDF_NESTING;
/// use ikigai_store::limits::{check_sparql, MAX_SPARQL_NESTING};
///
/// assert!(check_sparql("SELECT * WHERE { ?s ?p ?o FILTER((1)) }", "query").is_ok());
/// let deep = format!("SELECT * WHERE {{ FILTER({}1{}) }}", "(".repeat(100), ")".repeat(100));
/// let refusal = check_sparql(&deep, "query").unwrap_err().to_string();
/// assert!(refusal.contains("`query`") && refusal.contains(&MAX_SPARQL_NESTING.to_string()));
/// // Brackets in a string, an IRI or a comment are not nesting.
/// let quoted = format!("SELECT * WHERE {{ ?s ?p \"{}\" }} # {}", "(".repeat(100), "[".repeat(100));
/// assert!(check_sparql(&quoted, "query").is_ok());
/// // A triple-term template nests to the depth a load allows, and no further.
/// let template = |n: usize| {
///     format!(
///         "INSERT {{ <urn:a> <urn:p> {}?t{} }} WHERE {{ <urn:a> <urn:p> ?t }}",
///         "<<( <urn:s> <urn:p> ".repeat(n),
///         " )>>".repeat(n)
///     )
/// };
/// assert!(check_sparql(&template(MAX_RDF_NESTING), "content").is_ok());
/// let refusal = check_sparql(&template(MAX_RDF_NESTING + 1), "content").unwrap_err().to_string();
/// assert!(refusal.contains("`content`") && refusal.contains("MAX_RDF_NESTING"));
/// ```
pub fn check_sparql(text: &str, arg: &str) -> Result<()> {
    if text.len() > MAX_SPARQL_BYTES {
        return Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: format!(
                "this SPARQL text is {} bytes, and this store refuses any over {MAX_SPARQL_BYTES} \
                 (MAX_SPARQL_BYTES) before parsing it: the parser and evaluator are recursive, \
                 and the bound is what keeps the stack a query can claim finite. Split the \
                 work into several requests",
                text.len()
            ),
        });
    }
    let depth = deepest(text.as_bytes()).map_err(|ambiguous| refuse_ambiguous(arg, ambiguous))?;
    let depth = match unescaped(text) {
        Some(unescaped) => depth.max(
            deepest(unescaped.as_bytes()).map_err(|ambiguous| refuse_ambiguous(arg, ambiguous))?,
        ),
        None => depth,
    };
    if depth.brackets > MAX_SPARQL_NESTING {
        return Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: format!(
                "this SPARQL text nests its brackets deeper than {MAX_SPARQL_NESTING} \
                 (MAX_SPARQL_NESTING), counting `(`, `{{`, `[` and each `!` of a run, \
                 outside strings, IRIs and comments. It is refused before parsing because the parser recurses once \
                 per level and a stack overflow aborts the whole host, not one request. Real \
                 queries nest well under 10; flatten this one"
            ),
        });
    }
    if depth.terms > MAX_RDF_NESTING {
        return Err(Error::InvalidArgument {
            name: arg.to_string(),
            detail: format!(
                "this SPARQL text nests RDF 1.2 triple terms deeper than {MAX_RDF_NESTING} \
                 (MAX_RDF_NESTING), counting `<<( … )>>` and `<< … >>` outside strings, IRIs \
                 and comments: the depth a loaded document may nest them. It is refused before \
                 parsing because the parser and the store recurse once per level and a stack \
                 overflow aborts the whole host, not one request. Nest terms less deeply, or \
                 reify instead"
            ),
        });
    }
    Ok(())
}

fn refuse_ambiguous(arg: &str, readings: usize) -> Error {
    Error::InvalidArgument {
        name: arg.to_string(),
        detail: format!(
            "this SPARQL text can be read more than {readings} ways at once (MAX_SCAN_BRANCHES): \
             a `<` inside parentheses is either an IRI or a less-than, and this text keeps too \
             many of those readings open for its nesting to be bounded before parsing. Put a \
             space after a less-than `<`, or write IRIs containing `'`, `#` or brackets with a \
             PREFIX"
        ),
    }
}

/// Run `work` — a parse and everything that evaluates what it parsed — on a thread whose
/// stack is sized for `text` ([`sparql_stack_size`]), and wait for it.
///
/// A panic on that thread is resumed on this one, so the caller sees what it would have
/// seen inline. A thread that cannot be spawned (the address space or the platform refused
/// a stack this size) is a transient [`Error::Unavailable`], never a fallback to running
/// inline — inline is the overflow this exists to prevent. On wasm there are no threads,
/// and `work` runs inline.
///
/// ⚠ **Only what runs ON this thread gets its stack** (ledger #1122). Work that `work` hands
/// to another thread — a rayon pool, a spawned thread or task, an async executor, a channel to
/// a worker — recurses on THAT thread's stack, usually 2 MiB, and the sizing here does
/// nothing for it. `ikigai-shacl` was misled exactly so (ledger #1119): rudof's validator
/// moves work onto rayon's global pool whenever a shape level has two or more shapes, so the
/// sized thread protected nothing and a 3,000-term chain aborted the process. Keep every
/// parse and every evaluation of the text inside `work` itself, or run the pool on threads
/// sized with [`sparql_stack_size`].
///
/// ```
/// use ikigai_store::limits::on_sparql_stack;
///
/// let (here, spawned) = on_sparql_stack("SELECT * WHERE { ?s ?p ?o }", || {
///     let here = std::thread::current().name().map(str::to_string);
///     let spawned = std::thread::spawn(|| std::thread::current().name().map(str::to_string))
///         .join()
///         .unwrap();
///     Ok((here, spawned))
/// })
/// .unwrap();
/// assert_eq!(here.as_deref(), Some("ikigai-store-sparql")); // the sized thread
/// assert_eq!(spawned, None); // not the sized thread: a default stack
/// ```
///
/// ⚠ And it spawns a NEW thread per call, so it is the wrong place to COMMIT a write to a
/// RocksDB-backed store over and over: RocksDB seeds a thread-local generator from the
/// thread's id, a new thread usually reuses the last one's id, and repeated commits from
/// such threads degrade the memtable for every later read (ledger #1127; this crate's own
/// updates run on one long-lived writer thread for that reason — see the README).
pub fn on_sparql_stack<T, F>(text: &str, work: F) -> Result<T>
where
    T: Send,
    F: FnOnce() -> Result<T> + Send,
{
    #[cfg(not(target_family = "wasm"))]
    {
        let stack = sparql_stack_size(text);
        std::thread::scope(|scope| {
            let handle = std::thread::Builder::new()
                .name("ikigai-store-sparql".to_string())
                .stack_size(stack)
                .spawn_scoped(scope, work)
                .map_err(|e| {
                    Error::Unavailable(format!(
                        "could not start a {} MiB thread to parse this SPARQL text on: {e}",
                        stack >> 20
                    ))
                })?;
            match handle.join() {
                Ok(result) => result,
                Err(panic) => std::panic::resume_unwind(panic),
            }
        })
    }
    #[cfg(target_family = "wasm")]
    {
        let _ = text;
        work()
    }
}

/// The text with spargebra's `standard-unicode-escaping` applied, or `None` when it has no
/// `\u` / `\U` escape and would be unchanged.
fn unescaped(text: &str) -> Option<String> {
    if !text.contains("\\u") && !text.contains("\\U") {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let width = match after.as_bytes().first() {
            Some(b'u') => 4,
            Some(b'U') => 8,
            _ => 0,
        };
        let decoded = (width > 0)
            .then(|| after.get(1..=width))
            .flatten()
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .and_then(char::from_u32);
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &after[1 + width..];
            }
            None => {
                out.push('\\');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Some(out)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Lex {
    Code,
    Comment,
    /// A string literal opened by `quote`; `long` for the triple-quoted forms.
    Str {
        quote: u8,
        long: bool,
    },
    /// Inside a `<…>` read as an IRI, until the `>`.
    Iri,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Reading {
    lex: Lex,
    /// Bytes still to pass over without reading them (an escape's second byte, the rest of
    /// a `'''`, the second `<` of `<<`, the `<(` of `<<(`, the `>>` of `)>>`).
    skip: u8,
    /// The open brackets, innermost last; `<` stands for `<<` and `T` for `<<(`.
    open: Vec<u8>,
    /// How many of `open` are triple terms (`<` or `T`): they count toward
    /// [`crate::depth::MAX_RDF_NESTING`], the rest toward [`MAX_SPARQL_NESTING`].
    terms: usize,
    /// The last byte of code read, whitespace and comments aside — enough to know whether
    /// what came before a `<` could have been a less-than's left operand.
    last: u8,
    /// The `!`s in the run being read. spargebra parses `!!!x` as nesting — the one prefix
    /// operator it recurses on (`-`, `+` and a path's `^` do not repeat) — so a run counts
    /// toward the depth like brackets do, for as long as it lasts.
    bangs: usize,
}

/// How deep a reading of a text nests, in the two counts [`check_sparql`] bounds.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Depth {
    /// `(`, `{` and `[`, and each `!` of a run: bounded by [`MAX_SPARQL_NESTING`].
    brackets: usize,
    /// `<<( … )>>` triple terms and `<< … >>` reified triples: bounded by
    /// [`crate::depth::MAX_RDF_NESTING`], the depth a loaded document may nest them.
    terms: usize,
}

impl Depth {
    fn max(self, other: Depth) -> Depth {
        Depth {
            brackets: self.brackets.max(other.brackets),
            terms: self.terms.max(other.terms),
        }
    }

    fn over(self) -> bool {
        self.brackets > MAX_SPARQL_NESTING || self.terms > crate::depth::MAX_RDF_NESTING
    }
}

/// The deepest any reading of `text` nests, in each count, stopping early once one passes
/// its bound. `Err(n)` when more than `n` readings were alive at once.
fn deepest(text: &[u8]) -> std::result::Result<Depth, usize> {
    let mut readings = vec![Reading {
        lex: Lex::Code,
        skip: 0,
        open: Vec::new(),
        terms: 0,
        last: b'(',
        bangs: 0,
    }];
    let mut forks: Vec<Reading> = Vec::new();
    let mut deepest = Depth::default();
    for (at, &byte) in text.iter().enumerate() {
        for reading in readings.iter_mut() {
            step(reading, text, at, byte, &mut forks);
        }
        if !forks.is_empty() {
            readings.append(&mut forks);
        }
        if readings.len() > 1 {
            readings.sort_unstable();
            readings.dedup();
            if readings.len() > MAX_SCAN_BRANCHES {
                return Err(MAX_SCAN_BRANCHES);
            }
        }
        for reading in &readings {
            deepest = deepest.max(Depth {
                brackets: reading.open.len() - reading.terms + reading.bangs,
                terms: reading.terms,
            });
        }
        if deepest.over() {
            break;
        }
    }
    Ok(deepest)
}

fn step(reading: &mut Reading, text: &[u8], at: usize, byte: u8, forks: &mut Vec<Reading>) {
    if reading.skip > 0 {
        reading.skip -= 1;
        return;
    }
    let next = |n: usize| text.get(at + n).copied();
    match reading.lex {
        Lex::Comment => {
            if byte == b'\n' || byte == b'\r' {
                reading.lex = Lex::Code;
            }
        }
        Lex::Iri => {
            if byte == b'>' {
                reading.lex = Lex::Code;
            }
        }
        Lex::Str { quote, long } => match byte {
            b'\\' => reading.skip = 1,
            // A short string cannot hold a line break; spargebra fails there, and reading
            // on as code can only count more, never less.
            b'\n' | b'\r' if !long => reading.lex = Lex::Code,
            b if b == quote && !long => reading.lex = Lex::Code,
            b if b == quote && next(1) == Some(quote) && next(2) == Some(quote) => {
                reading.lex = Lex::Code;
                reading.skip = 2;
            }
            _ => {}
        },
        Lex::Code => {
            match byte {
                // A comment inside a run does not end it.
                b if b <= b' ' || b == b'#' => {}
                b'!' if next(1) != Some(b'=') => reading.bangs += 1,
                _ => reading.bangs = 0,
            }
            code(reading, text, at, byte, forks);
        }
    }
}

fn code(reading: &mut Reading, text: &[u8], at: usize, byte: u8, forks: &mut Vec<Reading>) {
    let next = |n: usize| text.get(at + n).copied();
    match byte {
        b if b <= b' ' => {}
        b'#' => reading.lex = Lex::Comment,
        // A prefixed name's `\(`, `\#`, `\'`… escapes one character into the name, and a
        // name is an operand whatever the character was.
        b'\\' => {
            reading.skip = 1;
            reading.last = b'a';
        }
        b'\'' | b'"' => {
            let long = next(1) == Some(byte) && next(2) == Some(byte);
            reading.lex = Lex::Str { quote: byte, long };
            reading.last = byte;
            if long {
                reading.skip = 2;
            }
        }
        b'(' | b'{' | b'[' => {
            reading.open.push(byte);
            reading.last = byte;
        }
        // `)>>` is one token: the closer of a `<<(` triple term.
        b')' if next(1) == Some(b'>')
            && next(2) == Some(b'>')
            && reading.open.last() == Some(&b'T') =>
        {
            reading.open.pop();
            reading.terms -= 1;
            reading.skip = 2;
            reading.last = b'>';
        }
        b')' => close(reading, b'('),
        b'}' => close(reading, b'{'),
        b']' => close(reading, b'['),
        b'>' if next(1) == Some(b'>') && reading.open.last() == Some(&b'<') => {
            reading.open.pop();
            reading.terms -= 1;
            reading.skip = 1;
            reading.last = byte;
        }
        b'<' => {
            // Every reading the grammar allows at a `<`. This one becomes the first
            // that applies; the rest are pushed as forks.
            let mut alternatives: Vec<Reading> = Vec::with_capacity(3);
            if could_be_iri(text, at) {
                alternatives.push(Reading {
                    lex: Lex::Iri,
                    skip: 0,
                    open: reading.open.clone(),
                    terms: reading.terms,
                    last: b'>',
                    bangs: 0,
                });
            }
            if next(1) == Some(b'<') {
                // `<<(` is ONE token, a triple term's opener with no space allowed inside it
                // (spargebra's `TripleTerm`, `TripleTermData` and `ExprTripleTerm`): one level,
                // and its `(` is not an expression's, so no less-than reading opens inside it
                // until a real `(` does. Read as `<<` then `(`, every level kept one more
                // reading of `<` alive and the scan refused 7 levels as ambiguous (ledger
                // #1354). `<<` then `(` has no reading of its own: nothing a reified triple
                // holds starts with `(`.
                let term = next(2) == Some(b'(');
                let mut open = reading.open.clone();
                open.push(if term { b'T' } else { b'<' });
                alternatives.push(Reading {
                    lex: Lex::Code,
                    skip: if term { 2 } else { 1 },
                    open,
                    terms: reading.terms + 1,
                    last: if term { b'(' } else { b'<' },
                    bangs: 0,
                });
            }
            // A less-than (or `<=`) needs an expression — and every SPARQL expression
            // sits directly inside `(` — and a left operand before it. With no other
            // reading left, keep this one anyway: the parser fails here, and reading on
            // can only count more.
            let less_than = reading.open.last() == Some(&b'(')
                && !NEVER_ENDS_AN_OPERAND.contains(&reading.last);
            if less_than || alternatives.is_empty() {
                let mut operator = reading.clone();
                operator.last = b'<';
                alternatives.push(operator);
            }
            let mut alternatives = alternatives.into_iter();
            if let Some(first) = alternatives.next() {
                *reading = first;
            }
            forks.extend(alternatives);
        }
        _ => reading.last = byte,
    }
}

/// Bytes after which no operand has just ended, so a `<` that follows cannot be a
/// less-than. Deliberately short — `-` and `.` are left out because a prefixed name can end
/// in one, and a byte left out only adds a reading.
const NEVER_ENDS_AN_OPERAND: &[u8] = b"(,=!&|+*/^<";

/// Pop the innermost bracket if `opener` is what it is. A closer that does not match is
/// one the parser fails on in this reading, so it closes nothing — never lowering the count.
fn close(reading: &mut Reading, opener: u8) {
    if reading.open.last() == Some(&opener) {
        reading.open.pop();
    }
    reading.last = b')';
}

/// Whether the `<` at `at` could open an IRI: spargebra takes `<` + anything but `>` + `>`
/// and keeps it only if the inside parses as an IRI, which no IRI does with whitespace, a
/// control character, `<`, `"`, `{` or `}` in it. Erring towards "could" is the safe
/// direction — it only adds a reading. The look-ahead stops at the first such byte, so the
/// scan stays linear.
fn could_be_iri(text: &[u8], at: usize) -> bool {
    for &byte in &text[at + 1..] {
        match byte {
            b'>' => return true,
            b if b <= b' ' => return false,
            b'<' | b'"' | b'{' | b'}' => return false,
            _ => {}
        }
    }
    false
}

/// Refuse an RDF document `urn:iki:store:load` was handed, before the store's writer is asked
/// for anything (ledger #1428): over `limit` bytes; RDF/XML declaring an XML entity; nesting
/// triple terms past [`MAX_RDF_NESTING`]; not parsing; or spelling out more than `limit` bytes
/// of terms. `parser` is the one the load will use (its format, and the graph it puts a
/// document's default graph in). See the module docs.
pub(crate) fn check_load(
    document: &[u8],
    parser: &oxigraph::io::RdfParser,
    limit: usize,
    label: &str,
    arg: &str,
) -> Result<()> {
    use oxigraph::io::RdfFormat;

    let refuse = |detail: String| Error::InvalidArgument {
        name: arg.to_string(),
        detail,
    };
    if document.len() > limit {
        return Err(refuse(format!(
            "this document is {} bytes, and this store loads none over {limit} \
             (MAX_LOAD_BYTES, or the bound its host set) before parsing it. Split it into \
             several loads, or ask the host for a larger bound. Nothing was loaded",
            document.len()
        )));
    }
    let format = parser.format();
    if format == RdfFormat::RdfXml && declares_an_entity(document) {
        return Err(refuse(
            "this RDF/XML document declares an XML entity (`<!ENTITY`), and this store loads \
             none: the RDF/XML parser expands an entity's value against the entities declared \
             before it, so a few hundred bytes of declarations can expand to gigabytes. Write \
             the text out in full, or use another syntax. Nothing was loaded"
                .to_string(),
        ));
    }
    crate::depth::check_rdf_nesting(document, format, arg)?;
    let mut produced = 0usize;
    for quad in parser.clone().for_slice(document) {
        let quad = quad.map_err(|e| refuse(format!("parsing {label}: {e}")))?;
        produced = produced
            .saturating_add(term_bytes(quad.subject.as_ref().into()))
            .saturating_add(quad.predicate.as_str().len())
            .saturating_add(term_bytes(quad.object.as_ref()))
            .saturating_add(match quad.graph_name.as_ref() {
                oxigraph::model::GraphNameRef::NamedNode(n) => n.as_str().len(),
                oxigraph::model::GraphNameRef::BlankNode(b) => b.as_str().len(),
                oxigraph::model::GraphNameRef::DefaultGraph => 0,
            });
        if produced > limit {
            return Err(refuse(format!(
                "this {} document's statements expand past {limit} bytes of terms (MAX_LOAD_BYTES, \
                 or the bound its host set), though the document is {} bytes: a prefix, a base \
                 IRI, a namespace or the `graph` argument is written once and copied into every \
                 statement that uses it. Use shorter names, or split the document. Nothing was \
                 loaded",
                format.name(),
                document.len()
            )));
        }
    }
    Ok(())
}

/// Whether an RDF/XML document declares an XML entity: `<!ENTITY` anywhere, in any case.
///
/// ★ Not a reading of the markup, on purpose. oxrdfxml takes a declaration only from text that
/// follows the bytes `<!ENTITY` inside a DOCTYPE (`parse_doctype` splits the DOCTYPE's text at
/// `<` and strips the prefix `!ENTITY`), and the depth scan already refuses a document that is
/// not UTF-8, so a document without those bytes declares nothing whatever a lexer would make
/// of it. A scan that agreed with quick-xml about where a DOCTYPE starts and ends would be one
/// more reading to keep in step. The cost is a document that merely MENTIONS `<!ENTITY` in a
/// comment or a CDATA section, which is refused too.
fn declares_an_entity(document: &[u8]) -> bool {
    const DECLARATION: &[u8] = b"<!entity";
    document
        .windows(DECLARATION.len())
        .any(|window| window.eq_ignore_ascii_case(DECLARATION))
}

/// The bytes of one term's text as the parser produced it: an IRI, a blank node's label, or a
/// literal's value with its language tag, or with its datatype IRI unless that is `xsd:string`.
fn term_bytes(term: oxigraph::model::TermRef<'_>) -> usize {
    use oxigraph::model::vocab::xsd;
    use oxigraph::model::TermRef;
    if let TermRef::NamedNode(n) = term {
        return n.as_str().len();
    }
    if let TermRef::BlankNode(b) = term {
        return b.as_str().len();
    }
    if let TermRef::Literal(l) = term {
        let tag = l.language().map_or(0, str::len);
        // A simple or language-tagged literal's datatype is a constant, not text it carries.
        let datatype = if l.language().is_some() || l.datatype() == xsd::STRING {
            0
        } else {
            l.datatype().as_str().len()
        };
        return l.value().len() + tag + datatype;
    }
    // An RDF 1.2 triple term, a variant that exists only in a build with `rdf-12` (see the gate
    // in `Cargo.toml`), so it is not named: its N-Triples text, which writing recurses once a
    // level, bounded by the depth check above.
    term.to_string().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deepest any reading nests its brackets.
    fn depth(text: &str) -> usize {
        deepest(text.as_bytes()).unwrap().brackets
    }

    /// The deepest any reading nests triple terms.
    fn terms(text: &str) -> usize {
        deepest(text.as_bytes()).unwrap().terms
    }

    #[test]
    fn brackets_of_every_kind_count_and_close_only_what_they_match() {
        assert_eq!(depth("SELECT * WHERE { ?s ?p ?o }"), 1);
        assert_eq!(depth("SELECT * WHERE { { [ <urn:p> ( 1 ) ] } }"), 4);
        // A closer of the wrong kind closes nothing: it cannot be used to look shallower.
        assert_eq!(depth("( ] ( ] ( ] ("), 4);
        assert_eq!(depth(") ) ) ( ( ("), 3);
    }

    #[test]
    fn strings_iris_and_comments_hide_brackets() {
        assert_eq!(depth(r#"{ ?s ?p "(((([[[[{{{{" }"#), 1);
        assert_eq!(depth(r"{ ?s ?p '(((' }"), 1);
        assert_eq!(depth(r#"{ ?s ?p """((( " "" ((( """ }"#), 1);
        assert_eq!(depth("{ ?s ?p '''((( ' '' ((('''}"), 1);
        assert_eq!(depth(r#"{ ?s ?p "\"(((" }"#), 1);
        assert_eq!(depth("{ ?s <http://x/a(b(c(d> ?o }"), 1);
        assert_eq!(depth("{ # ((((((\n ?s ?p ?o }"), 1);
        assert_eq!(depth("{ # ((((((\r ?s ?p ?o }"), 1);
        // A prefixed name's escaped bracket is part of the name.
        assert_eq!(depth(r"{ ?s ex:a\(\(\( ?o }"), 1);
    }

    #[test]
    fn a_less_than_reading_is_followed_where_an_expression_can_be() {
        // Inside `(`, `<…>` may be a less-than whose "IRI" is code: the scan counts both.
        let attack = "SELECT * WHERE { FILTER(?a<(((((1)))))>0) }";
        assert_eq!(depth(attack), 2 + 5);
        // A string opened INSIDE what an IRI reading would skip.
        let phase = format!(
            "SELECT * WHERE {{ FILTER({}1) }}",
            "( ?a<'> ) ' && ".repeat(20)
        );
        assert!(depth(&phase) >= 20, "{}", depth(&phase));
        // A comment opened inside one, hiding the closers on the rest of the line.
        let comment =
            "SELECT * WHERE { FILTER(\n".to_string() + &"(( ?a<#> )) ?a < \n".repeat(10) + "1) }";
        assert!(depth(&comment) >= 10, "{}", depth(&comment));
        // After an operator there is no left operand, so `<…>` is an IRI there too.
        assert_eq!(
            depth("SELECT * WHERE { FILTER(?o != <urn:x:((((((> && ?o = <urn:y#(>) }"),
            2
        );
        assert_eq!(
            depth("SELECT * WHERE { FILTER(\"a\"^^<urn:t((((> = ?o) }"),
            2
        );
        // …but a name ending in an escaped bracket is an operand, so not after one.
        assert_eq!(
            depth("SELECT * WHERE { FILTER(ex:a\\(<urn:x((((>) }"),
            2 + 4
        );
        // Outside `(`, `<…>` can only be an IRI, so a `#` or `(` in one opens nothing.
        let triple =
            "SELECT * WHERE {\n".to_string() + &"{ ?s <http://x/v#p> ?o }\n".repeat(200) + "}";
        assert_eq!(depth(&triple), 2);
    }

    #[test]
    fn a_run_of_not_nests_and_not_equal_does_not() {
        let run = format!("SELECT * WHERE {{ FILTER({}true) }}", "! #c\n!".repeat(30));
        assert_eq!(depth(&run), 2 + 60);
        assert_eq!(
            depth("SELECT * WHERE { FILTER(?a != ?b && !?c && !(!?d)) }"),
            2 + 2
        );
        // A path's `!` does not recurse, but counting it costs one level, never more.
        assert_eq!(depth("SELECT * WHERE { ?s !<urn:p> ?o }"), 2);
    }

    #[test]
    fn triple_terms_nest_and_the_unescaped_reading_is_scanned_too() {
        assert_eq!(depth("{ << << ?s ?p ?o >> ?q ?r >> ?x ?y }"), 1);
        assert_eq!(terms("{ << << ?s ?p ?o >> ?q ?r >> ?x ?y }"), 2);
        assert_eq!(terms("{ ?a ?b <<( <<( ?s ?p ?o )>> ?q ?r )>> }"), 2);
        let escaped = format!(
            "SELECT * WHERE {{ FILTER({}1{}) }}",
            r"\u0028".repeat(100),
            r"\u0029".repeat(100)
        );
        assert!(depth(&escaped) < MAX_SPARQL_NESTING);
        assert!(check_sparql(&escaped, "query").is_err());
    }

    /// An update whose `INSERT` template is a triple term `n` deep around `?t`, with `s p`
    /// as given, at the broad door's shape (`graph` false) or the graph door's.
    fn template(sp: &str, n: usize, graph: bool) -> String {
        let term = format!("{}?t{}", format!("<<( {sp} ").repeat(n), " )>>".repeat(n));
        if graph {
            format!(
                "PREFIX ex: <urn:ex:> DELETE {{ GRAPH <urn:g> {{ <urn:a> <urn:p> ?t }} }} \
                 INSERT {{ GRAPH <urn:g> {{ <urn:a> <urn:p> {term} }} }} \
                 WHERE {{ GRAPH <urn:g> {{ <urn:a> <urn:p> ?t }} }}"
            )
        } else {
            format!(
                "PREFIX ex: <urn:ex:> DELETE {{ <urn:a> <urn:p> ?t }} \
                 INSERT {{ <urn:a> <urn:p> {term} }} WHERE {{ <urn:a> <urn:p> ?t }}"
            )
        }
    }

    /// Ledger #1354: a 7-deep `<<( … )>>` template was refused as too ambiguous to bound
    /// (`MAX_SCAN_BRANCHES`, 2^7 readings: each `<<(`'s `(` read as an expression's kept a
    /// less-than reading of the next `<` alive), far below the depth a load allows. `<<(` is
    /// one token, so the template nests to `MAX_RDF_NESTING` and the refusal past it names
    /// that bound.
    #[test]
    fn a_triple_term_template_nests_to_the_rdf_bound_and_the_refusal_names_it() {
        for sp in [
            "ex:s ex:p",
            "<urn:s> <urn:p>",
            "<urn:s> a",
            "?s ?p",
            "\"s\" ex:p",
        ] {
            for graph in [false, true] {
                for n in [7, 8, MAX_RDF_NESTING] {
                    check_sparql(&template(sp, n, graph), "content")
                        .unwrap_or_else(|e| panic!("{sp} graph={graph} at {n}: {e}"));
                }
                let over = template(sp, MAX_RDF_NESTING + 1, graph);
                let err = check_sparql(&over, "content").unwrap_err().to_string();
                assert!(
                    err.starts_with("invalid argument `content`")
                        && err.contains("MAX_RDF_NESTING")
                        && !err.contains("MAX_SCAN_BRANCHES"),
                    "{sp} graph={graph}: {err}"
                );
            }
        }
        // In an expression too, where every `(` used to open a less-than reading.
        let expr = |n: usize| {
            format!(
                "SELECT * WHERE {{ BIND({}1{} AS ?x) FILTER(?x = {}?y{}) }}",
                "<<( <urn:s> <urn:p> ".repeat(n),
                " )>>".repeat(n),
                "<<( ?s <urn:p> ".repeat(n),
                " )>>".repeat(n)
            )
        };
        check_sparql(&expr(MAX_RDF_NESTING), "query").unwrap();
        let err = check_sparql(&expr(MAX_RDF_NESTING + 1), "query").unwrap_err();
        assert!(err.to_string().contains("MAX_RDF_NESTING"), "{err}");
    }

    #[test]
    fn triple_terms_and_brackets_are_counted_apart() {
        // 64 brackets (`{`, `FILTER(` and 62 more) around 64 triple terms: both at their bound.
        let both = |brackets: usize, terms: usize| {
            format!(
                "SELECT * WHERE {{ FILTER({}{}1{} = ?x{}) }}",
                "(".repeat(brackets - 2),
                "<<( <urn:s> <urn:p> ".repeat(terms),
                " )>>".repeat(terms),
                ")".repeat(brackets - 2)
            )
        };
        let at = both(MAX_SPARQL_NESTING, MAX_RDF_NESTING);
        assert_eq!(
            deepest(at.as_bytes()).unwrap(),
            Depth {
                brackets: MAX_SPARQL_NESTING,
                terms: MAX_RDF_NESTING
            }
        );
        check_sparql(&at, "query").unwrap();
        let err = check_sparql(&both(MAX_SPARQL_NESTING + 1, 1), "query").unwrap_err();
        assert!(err.to_string().contains("MAX_SPARQL_NESTING"), "{err}");
        let err = check_sparql(&both(3, MAX_RDF_NESTING + 1), "query").unwrap_err();
        assert!(err.to_string().contains("MAX_RDF_NESTING"), "{err}");
        // A bracket INSIDE a triple term (a built-in call in `INSERT DATA`) is a bracket.
        assert_eq!(
            deepest(b"INSERT DATA { <urn:a> <urn:p> <<( <urn:s> <urn:p> STR((1)) )>> }").unwrap(),
            Depth {
                brackets: 3,
                terms: 1
            }
        );
    }

    #[test]
    fn a_triple_term_closes_only_on_its_own_token() {
        // `) >>` is not `)>>`: the parser fails there, and the term stays open in the count.
        let split = "{ ".to_string() + &"<<( ex:s ex:p ?o ) >> ".repeat(5) + "}";
        assert_eq!(terms(&split), 5);
        // `)>>` closes a `<<(` and `>>` a `<<`, never the other, so neither can make a
        // reading look shallower than it is.
        assert_eq!(depth("{ ( ( )>> )>> ( }"), 3);
        assert_eq!(terms("{ <<( ?s ?p ?o >> <<( ?s ?p ?o >> }"), 2);
        // `<< (` with a space is `<<` and a `(`: one term level and one bracket.
        assert_eq!(
            deepest(b"{ << ( ?s ?p ?o ) >> }").unwrap(),
            Depth {
                brackets: 2,
                terms: 1
            }
        );
    }

    #[test]
    fn the_bound_is_the_number_the_integration_tests_restate() {
        // `tests/sparql_nesting.rs` must compile against 0.2.6 to reproduce the abort there,
        // so it cannot name this constant and says 64 instead.
        assert_eq!(MAX_SPARQL_NESTING, 64);
    }

    #[test]
    fn the_bounds_refuse_by_name_and_admit_at_the_bound() {
        let at = format!(
            "SELECT * WHERE {{ FILTER({}1{}) }}",
            "(".repeat(MAX_SPARQL_NESTING - 2),
            ")".repeat(MAX_SPARQL_NESTING - 2)
        );
        assert_eq!(depth(&at), MAX_SPARQL_NESTING);
        check_sparql(&at, "query").unwrap();
        let over = at.replacen('(', "((", 1).replacen(')', "))", 1);
        let err = check_sparql(&over, "content").unwrap_err();
        assert!(
            matches!(&err, Error::InvalidArgument { name, .. } if name == "content"),
            "{err}"
        );
        let big = format!(
            "SELECT * WHERE {{ VALUES ?x {{ {} }} }}",
            "1 ".repeat(MAX_SPARQL_BYTES / 2)
        );
        let err = check_sparql(&big, "query").unwrap_err().to_string();
        assert!(err.contains("MAX_SPARQL_BYTES"), "{err}");
    }
}
