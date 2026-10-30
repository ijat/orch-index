# orch-index plan

Working mode (2026-10-30): single session, one commit per working milestone. Code and security
reviews are deferred until we decide to publish. The benchmark is not deferred: it is how we prove
the tool works. Background and done criteria: `../HANDOFF.md`.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Rust workspace, toolchain pinned in `rust-toolchain.toml` | Handoff. |
| D2 | Read sources from the git object store (`git ls-tree` + `git cat-file --batch`), not the worktree | Works in sparse worktrees; the blob OID is a free content hash for incremental updates. |
| D3 | Incremental key = per-file blob OID. Update = diff the stored `(path, oid)` set against `ls-tree HEAD`, re-extract only changed paths | Simpler and more robust than `git diff A..B` (no need for the old commit to exist; shallow clones fine). |
| D4 | Every fact carries a `Provenance` (`syntactic` < `resolved` < `compiler`) | Agents must know when to trust a result (the lesson from ctags). |
| D5 | Layer 1 = tree-sitter, one `Extractor` impl per language behind a trait; C/C++ first | Firefox and Chromium are C++-dominated; that is where the benchmark questions are. |
| D6 | Storage = SQLite (`rusqlite`, bundled) behind a `Store` boundary; names interned into a `names` table | Measured: SQLite answered lookups in 0.08 s on 16M rows. Sharding only if measurements demand it. |
| D7 | Snippets are not stored; the query layer reads the line from the blob at query time | Keeps the DB small; output stays compact. |
| D8 | Query output: count line first, then `path:line  [prov]  snippet`; `--json` on request; results capped by default | Token budget for Haiku agents. |
| D9 | Include resolution behind a `Resolver` trait. First impl: path-suffix matching against the indexed tree, labelled `resolved` only when unique | The 70%-unresolved problem; build-system resolvers (moz.build EXPORTS, compile_commands, GN) slot in later. |
| D10 | Language order: C/C++ → Rust → JavaScript/TypeScript → C# (.NET). Others only if the benchmark shows the index is worth it | User decision 2026-10-30. Firefox carries Rust and JS too, so it can benchmark them. |

## Crates

| Crate | Role |
|---|---|
| `oi-model` | Fact types (`Symbol`, `Ref`, `Include`, `Base`), `Kind`, `Provenance`, `Lang`. No I/O. |
| `oi-git` | Git object-store source: list tree, stream blobs. |
| `oi-lang` | `Extractor` trait, language registry, tree-sitter C/C++ extractor. |
| `oi-store` | SQLite schema, writes, lookups. |
| `oi-query` | Questions (`def`, `refs`, `includers`, `overrides`) with provenance + notes; snippets from blobs. Include resolution lives here for now (exact/relative → `resolved`, `ambiguous:N`, `basename`). |
| `oi-indexer` | Pipeline: source → parallel extract → store; incremental update; include resolution. |
| `orch-index` (`crates/cli`) | Agent-facing CLI. |

## Milestones

- [x] **M1** Skeleton + C/C++ `def` / `refs` on a fixture repo; full and incremental indexing; tests.
- [x] **M2** `includers`, `callers`, `overrides` (bases + per-class methods, signature-aware). `impact <rev>` deferred until after M3.
- [ ] **M3** Firefox benchmark: index time/size, lookup latency, incremental timing; accuracy vs the
      2026-10-30 ground truth (Q1 includers of `nsHttpChannel.h` = 24; Q3 `OnStartRequest` implementers = 45);
      Haiku+index vs Haiku+grep on tokens and accuracy. Scripts + results in `bench/`.
- [ ] **M4** Fix what the numbers show. Known: 43% of Firefox C/C++ files have ≥1 tree-sitter error (mostly local, in bodies/args; ~4% top-level); consider labelling hits from files with top-level errors. Ask the user before any Chromium-scale clone.
- [ ] **M4b** Extractors for Rust, then JS/TS, then C#; each benchmarked before the next.
- [ ] **M5** orch integration through orch-skill's workflow (later).

## Log

- 2026-10-30: plan written.
- 2026-10-30: M1+M2 done: 7 crates, clippy pedantic clean, unit + end-to-end tests (fixture git repo, incremental update).
- 2026-10-30: Firefox @0b6146da78 (41,857 C/C++ files): full index 41 s, 1.0 GB RSS, 1.04 GB DB (20.4M refs);
  no-op update 0.92 s. Q1 includers of nsHttpChannel.h 24/24 exact (all `resolved`, 27 ms); Q3
  OnStartRequest(nsIRequest*) implementers under netwerk/ 45/45 (49 ms). Fixed: statement macros
  without `;` (NS_IMPL_ISUPPORTS...) mis-scoped the rest of the file → blanked before parsing.
