# orch-index

A code index that makes very large git repositories (Chromium scale) queryable by coding agents and
humans: where a symbol is defined, who calls or includes it, which classes implement a method.
Answers are compact `path:line` rows, and each row carries a trust label that says how much to
believe it.

It needs no build. It reads source straight from the git object store, parses it with tree-sitter,
and stores facts in a local SQLite file. Nothing is sent anywhere.

Languages: C/C++ (Objective-C++ best effort), Rust, JavaScript, TypeScript (incl. JSX/TSX), C#.
Mixed-language repositories go into one index.

## Why

`grep` is fine on a small repository. On a large one, an agent asking "which classes implement
`OnStartRequest`?" burns tokens on search output and still misses answers (hits in unrelated class
hierarchies, same-signature methods, qualified names). A pre-built index answers in about 20 ms with
a short, exact, labelled list.

Measured on Firefox and Chromium (`bench/RESULTS.md`, 2026-10-30; Haiku answering four structural
questions per repo, 3 runs per cell, grep-only vs with the index):

| | Firefox @ 0b6146da78 | Chromium `src` @ 0b2cf11831 |
|---|---|---|
| Full index | 41 s, 1.0 GB peak RSS | 112 s, 1.1 GB peak RSS |
| No-op update | 0.96 s | 1.8 s warm (4.7 s cold) |
| Lookup, best of 5 (process start included) | 18–19 ms | 19–35 ms |
| Index alone vs ground truth (4 questions) | 4/4 exact | 4/4 exact |
| Haiku+index vs Haiku+grep, exact runs | 12/12 vs 12/12 | **12/12 vs 10/12** |
| Haiku tokens, sum of per-question means | **158K vs 337K (−53%)** | **162K vs 351K (−54%)** |

Those first numbers were measured when only C/C++ was indexed. After later fixes the index covers all
five languages, and a full index of the same commits takes 105 s (Firefox) and 170 s (Chromium);
see `bench/RESULTS.md` for the re-run. On questions that grep answers with one search, the index is
roughly a tie in tokens; on inheritance questions it uses 72% fewer tokens and is about 3× faster.

## Install

Requires `git` on `PATH` (it is used to list trees and stream blobs).

### Prebuilt binaries

Each [GitHub Release](https://github.com/ijat/orch-index/releases) has archives named
`orch-index-<tag>-<target>.tar.gz` (`.zip` for Windows), plus `SHA256SUMS`:

| Target | Platform |
|---|---|
| `aarch64-apple-darwin` | macOS, Apple Silicon |
| `x86_64-apple-darwin` | macOS, Intel |
| `x86_64-unknown-linux-musl` | Linux x86_64 (static, any distro) |
| `aarch64-unknown-linux-musl` | Linux arm64 (static, any distro) |
| `x86_64-pc-windows-msvc` | Windows x86_64 |

macOS and Linux:

```sh
VERSION=v0.1.0
TARGET=aarch64-apple-darwin     # pick a target from the table
BASE=https://github.com/ijat/orch-index/releases/download/$VERSION
ARCHIVE=orch-index-$VERSION-$TARGET.tar.gz

curl -fsSLO "$BASE/$ARCHIVE"
curl -fsSLO "$BASE/SHA256SUMS"
grep " $ARCHIVE\$" SHA256SUMS | shasum -a 256 -c -     # on Linux: sha256sum -c -
tar -xzf "$ARCHIVE"
install -m 755 "orch-index-$VERSION-$TARGET/orch-index" /usr/local/bin/   # or any directory on PATH
orch-index --version
```

Windows (PowerShell):

```powershell
$Version = "v0.1.0"
$Archive = "orch-index-$Version-x86_64-pc-windows-msvc.zip"
Invoke-WebRequest "https://github.com/ijat/orch-index/releases/download/$Version/$Archive" -OutFile $Archive
Expand-Archive $Archive -DestinationPath .
# put orch-index-$Version-x86_64-pc-windows-msvc\orch-index.exe on your PATH
```

Release archives carry build-provenance attestations. To check that one was built by this
repository's release workflow:

```sh
gh attestation verify "$ARCHIVE" --repo ijat/orch-index
```

The Windows build is checked by CI (tests run on Windows), but it has had far less real-world use
than the macOS and Linux builds.

### With cargo

```sh
cargo install --git https://github.com/ijat/orch-index orch-index
```

### From source

You need a C compiler (the tree-sitter grammars and the bundled SQLite are C) and `rustup`. The Rust
toolchain version is pinned in `rust-toolchain.toml`, and `rustup` installs it on first use.

```sh
git clone https://github.com/ijat/orch-index
cd orch-index
cargo build --release          # binary: target/release/orch-index
```

## Quick start

```sh
cd path/to/your/repo
orch-index index                       # build, or update incrementally to HEAD
orch-index def nsHttpChannel --kind class
orch-index callers NS_GetFinalChannelURI
```

The first `index` parses every supported file in the committed tree; later runs re-extract only
changed blobs. Queries read the index only and answer in milliseconds.

## Commands

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

| Command | Answers |
|---|---|
| `index` | Build or update the index (`--rev REV`, default `HEAD`). |
| `def NAME` | Where a symbol is defined or declared. `NAME` may be qualified: `Class::Method`. `--kind class,struct,function,method,macro,...` restricts kinds. |
| `callers NAME` | Call sites of a function or method, with the calling function. |
| `refs NAME` | Every use of a name, by name (not type-resolved). `--kind call\|type\|use`. |
| `includers FILE` | Files that `#include` the given repository path (also relative JS/TS imports). |
| `importers MODULE` | Files importing a module or namespace: Rust `use`, C# `using`, JS/TS packages. |
| `overrides METHOD` | Classes declaring or defining a method, one row per class. `--sig "(nsIRequest*)"` restricts the parameter list; `--base CLASS` keeps classes deriving (transitively) from `CLASS`. |
| `stats` | Row counts and the indexed commit. |

Common flags: `--path PREFIX`, `--lang cpp|rust|javascript|typescript|csharp`, `--limit N` (default
30; the rest is summarized per directory; `0` = no limit), `--files` (one row per file), `--json`,
`--no-snippet`. Global flags: `--repo DIR` (any directory inside the repository, worktrees
included) and `--db FILE` (default `<git-common-dir>/orch-index/index.db`). `orch-index --help` and
`orch-index <command> --help` list everything.

## Output

```
1 definitions of Extractor
crates/oi-lang/src/lib.rs:32 [syntactic] interface | pub trait Extractor: Send + Sync {
```

A count line, then one row per hit: `path:line [label] detail | source line`. With `--files`, rows are
`path (hits) [label]`. With `--json`, the same data is machine-readable. Source lines come from the
indexed blob, so they match the indexed commit even when your working tree has moved on. If the
index is behind `HEAD` or was built by an older version, the count line says so and tells you to run
`orch-index index`.

### Trust labels

| Label | Meaning |
|---|---|
| `resolved` | Exact: e.g. an include spec that names this file and no other. |
| `syntactic` | Parsed from source; names are not type-resolved. |
| `syntactic:ambiguous:N` | The include spec matches N files, this one among them. |
| `syntactic:basename` | Only the file name matches (typically a build-system export path). |
| `…:parse-errors` | The file has declaration-level syntax errors; facts from it may be incomplete. |
| `compiler` | Reserved for compiler-backed data (SCIP, Kythe). Nothing emits it yet. |

## Where the index lives, and updates

The default database is `<git-common-dir>/orch-index/index.db`: inside `.git`, shared by every
worktree of the repository, never in the working tree. Use `--db FILE` to put it elsewhere. Index
size depends on the repository: 2.24 GB for Firefox and 3.01 GB for Chromium `src`, both indexed with
all five languages (`bench/RESULTS.md`).

Facts are keyed by git blob id. `orch-index index` lists the target tree, extracts only blobs it
has not seen, and drops facts for removed files, so a no-op update takes about a second on
Chromium. The index covers the committed tree at the indexed revision, not uncommitted edits.
Sources are read from the git object store, so a sparse worktree is indexed in full, and every hit's
source line comes from the indexed blob.

Concurrent `index` runs on one index wait for each other. The index records which extractor
version built it: after an upgrade, queries report the index as stale until `orch-index index`
runs again.

## Robustness

The indexed repository is treated as untrusted input:
- blobs over 4 MiB are skipped without being buffered in memory;
- each file's walk is capped at depth 1024 and 5 s, and files cut short are labelled `parse-errors`;
- non-UTF-8 paths are skipped;
- extractor panics fail only their own file;
- every printed field has control characters escaped, so file names and contents cannot forge
  result rows or send terminal escapes.

## Languages and limits

Extractors are build-free and syntactic: they parse source with tree-sitter and never run a
compiler, resolve types, or evaluate the preprocessor or build system.

- C/C++ by extension (`.c .h .cc .cpp .cxx .hh .hpp .inl ...`, and `.mm` as best-effort C++);
  Rust (`.rs`); JavaScript (`.js .mjs .cjs .jsx`); TypeScript (`.ts .tsx`); C# (`.cs`).
- References match by name (`syntactic`). For generic names such as `Init`, filter with `--path` or
  read the calling function in the detail column.
- Macros are not expanded. Common statement-like and export macros in C/C++ are blanked before
  parsing; unusual ones can still hide a declaration.
- 24% of Chromium and 41% of Firefox C/C++ files have at least one tree-sitter error. Most are local
  and harmless. Hits from files with declaration-level errors are labelled `parse-errors`.
- Objective-C(++) is parsed with the C++ grammar: best effort.
- Cross-language links (JS↔C++ bindings, FFI, IDL) are not modelled. In a mixed repository,
  name-based references can mix languages; filter with `--lang`.
- No compiler-backed data yet; SCIP ingestion is a possible later step.

## Use with coding agents

`orch-index` is a plain CLI, so any agent that can run shell commands can use it: tell it to run
`orch-index def|callers|refs|includers|importers|overrides` before searching with `grep`, and to
trust `[resolved]` rows as they are. `bench/index-arm.md` is the exact prompt snippet the benchmark
used. Use `--json` for tool wrappers.

The [orch skill](https://github.com/ijat/orch), a multi-agent orchestration workflow for Claude Code,
is one intended consumer. `orch-index` has no dependency on it.

## Benchmarks

`bench/` holds the questions (`questions*.json`), the independently built ground truth
(`bench/truth/`), the Haiku A/B harness (`haiku_ab.py`) and the timing script (`index_timing.sh`).
Results and the story of what the real corpora broke: `bench/RESULTS.md`.

To reproduce:
- Timing and accuracy: build with `cargo build --release`, clone a corpus at the commit named in
  `questions*.json`, run `bench/index_timing.sh CORPUS_DIR`, then score the index alone with
  `bench/index_accuracy.py --corpus CORPUS_DIR --questions bench/questions-<repo>.json`. Both
  scripts use `target/release/orch-index`.
- Haiku A/B: `bench/haiku_ab.py --corpus DIR --bin DIR --out DIR` needs the Claude Code CLI and the
  `orch-explorer` agent role. By default it reads `~/.claude/agents/orch-explorer.md` (the role from
  the orch skill); pass `--role FILE` to use another role file. It runs agents with Bash inside the
  corpus, so use it only on corpora you trust. It makes real model calls and costs money.

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

## License

MIT. See `LICENSE`.
