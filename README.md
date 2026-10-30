# orch-index

A code index that makes very large git repositories (Chromium scale) queryable by agents: where a
symbol is defined, who calls or includes it, which classes implement a method. Answers are compact
`path:line` rows, and each one carries a trust label.

Languages: C/C++ (Objective-C++ best effort), Rust, JavaScript, TypeScript (incl. JSX/TSX), C#.
Mixed-language repositories go into one index. Results: `bench/RESULTS.md`.

## Build

```sh
cargo build --release          # toolchain pinned in rust-toolchain.toml
```

## Use

```sh
orch-index index                       # build, or update incrementally to HEAD (re-extracts changed blobs only)
orch-index def nsHttpChannel --kind class
orch-index callers NS_GetFinalChannelURI
orch-index includers netwerk/protocol/http/nsHttpChannel.h   # C/C++ includes, relative JS/TS imports
orch-index importers internal/validators          # module imports: Rust use, C# using, JS/TS packages
orch-index overrides OnStartRequest --sig "(nsIRequest*)" --path netwerk/
orch-index refs nsIRequest --kind type --path dom/
orch-index stats
```

Common flags: `--path PREFIX`, `--lang cpp|rust|javascript|typescript|csharp`, `--limit N` (default
30; the rest is summarized per directory), `--files` (one row per file), `--json`, `--no-snippet`,
`--repo DIR`, `--db FILE` (default `<git-common-dir>/orch-index/index.db`).

Sources are read from the git object store, so the index covers the whole committed tree even in a
sparse worktree, and every hit's source line comes from the indexed blob.

## Trust labels

| Label | Meaning |
|---|---|
| `resolved` | Exact: e.g. an include spec that names this file and no other. |
| `syntactic` | Parsed from source; names are not type-resolved. |
| `syntactic:ambiguous:N` | The include spec matches N files, this one among them. |
| `syntactic:basename` | Only the file name matches (typically a build-system export path). |
| `…:parse-errors` | The file has declaration-level syntax errors; facts from it may be incomplete. |
| `compiler` | Reserved for compiler-backed data (SCIP, Kythe). |

## Layout

| Crate | Role |
|---|---|
| `oi-model` | Fact types and provenance. |
| `oi-git` | Tree listing and blob streaming from git. |
| `oi-lang` | `Extractor` trait, shared tree walker, extractors for C/C++, Rust, JS/TS, C#. |
| `oi-store` | SQLite schema and lookups. |
| `oi-indexer` | Incremental pipeline (blob-id keyed, parallel extraction). |
| `oi-query` | Questions, confidence classification, snippets. |
| `cli` | The `orch-index` binary. |

## Benchmarks

`bench/` holds the questions, the independently built ground truth, the Haiku A/B harness
(`haiku_ab.py`) and the timing script (`index_timing.sh`). Results: `docs/PLAN.md` log and
`bench/RESULTS.md`.
