# orch-index

A general code indexer that makes very large repositories (Chromium scale) queryable by orch agents.
**Not started.** This folder holds the proposal until the work begins.

## Why

The Firefox benchmark (2026-10-30, 486K files; results in `~/.claude/benchmarks/bigrepo-2026-10-30/RESULTS.md`)
found:

- Haiku with scoped grep answers exact-text and simple semantic questions correctly at Firefox scale.
  It breaks down with scale: whole-tree searches take about 10 s and return about 100 KB.
- Heuristic indexes (ctags) are not trusted: they missed 9 of 45 definitions and treated overloads
  as overrides. In the benchmark, Haiku spent *more* tokens with the index, because it had to verify it.
- A flat tags file does not scale (3.3 GB, 12 s per lookup). The same data in SQLite answers a lookup
  in 0.08 s, and re-indexing 200 changed files takes 0.9 s.
- Compiler-accurate indexers at this scale all need a build: Kythe (Chromium code search), Searchfox's
  Clang plugin (Firefox), scip-clang (needs `compile_commands.json`).

## Proposed shape

- Rust.
- tree-sitter as the baseline for every language, with no build needed.
- SCIP ingestion for precise data where a compiler index exists.
- Sharded SQLite storage.
- Incremental updates keyed on git SHAs.
- A query CLI with compact `file:line` output, called by an `orch-graph` (Haiku) agent.
