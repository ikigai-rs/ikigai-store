# What this store owes the cache-ejection design

**Status:** design only, 2026-09-13. Nothing exports, nothing imports, there is no format
and no version byte. This note exists so that the *next* arc does not have to re-derive
which of `ikigai-core/docs/design/cache-ejection.md`'s constraints land on this crate —
and so that a future change here does not quietly close a door that was deliberately left
open.

## The settled part

> I want to be able to someday eject the cache into a serialized form. That could be
> useful as a way of pre-calculating things and letting another instance benefit from
> that work. — Brian, 2026-09-12

The obvious reading — two instances share one store — **collides with this crate's
central constraint**: RocksDB permits one writer per directory, and a read-only opener is
a frozen snapshot (`handle-model.md`). Brian settled it on 2026-09-13: **the bundle is a
FILE the second instance imports into its own store.** Not a shared store, and not a
read-only opener. That is also the only option consistent with the ejection design's own
requirements, every one of which assumes a boundary is being crossed.

## What that means for this crate, concretely

An import is a **capability-gated write of untrusted input that must cut a thread**.
Every one of those three is already true of `urn:iki:store:load`, which is the whole
reason to record this now rather than later:

| ejection design | how this store already satisfies it |
| --- | --- |
| §2 the key's capability half must be re-derived from the **importing** caller, never adopted from the bundle | an import is a write, and a write requires `urn:cap:store:write` held by the *local* caller. Nothing in a bundle can grant it |
| §4 policy metadata must be re-based locally | nothing here carries a foreign tick or hit count; a loaded document becomes quads and nothing else |
| §5 a bundle is untrusted input | `urn:iki:store:load` treats its `content` as untrusted already — a parse failure is `InvalidArgument`, naming the argument, not an endpoint error |
| §1 an imported entry must be invalidatable | an import cuts `urn:iki:store:load`, so every cacheable read of this store recomputes. An import that did NOT cut would be the stale-forever shape twice over |

**§3 is the one this crate cannot satisfy and must not pretend to.** The cache key does
not identify the code that produced an entry, so a bundle has to carry, per entry, the
resolved endpoint's `Description::id` and the producing crate's version, checked against
the local binding at import. That is a claim *about* entries, not a set of quads, and it
does not belong in a `load` of an RDF document.

## The shape that would fit, if someone builds it

Two observations, offered as a starting point rather than a decision:

1. **The manifest is RDF-shaped and the payload is not.** Producer identity, per-entry
   endpoint id and version, `ContentId`s, an `ikigai-core` version — that is a graph, and
   `ikigai-sign` already signs graphs, which is what §5 asks for. The representations
   themselves are opaque bytes and are not.
2. **So the natural split is: the manifest graph lands in this store through the existing
   gated, thread-cutting `urn:iki:store:load`; the payload lands in the kernel's cache
   through something in `ikigai-core`, admitted only for entries the manifest vouches
   for.** That keeps the untrusted-input surface here to "does this parse as RDF", which
   is a surface this crate can actually defend.

⚠ **What must not happen**: an `urn:iki:store:import-cache` that takes a bundle and does
both halves. It would put cache-validity logic — the §1/§2/§3 checks, which are about the
kernel's own key and capability model — inside an RDF store, where nothing that changes
the key would ever notice it had to change here too.

## What would reopen this

- A `Thread` that can produce a **witness** (a content digest, a store revision) rather
  than a per-process generation counter. `cache-ejection.md` §1 wants it for cross-process
  imports; `handle-model.md` wants it for an honest read-only face. One mechanism, two
  callers — which is the usual sign that it is the real missing piece.
