# Results

Machine: Apple Silicon Mac, APFS, warm page cache unless noted. Raw run files live outside the repo
in `~/.claude/benchmarks/orch-index-2026-10-30/` because they contain local paths. Both corpora were
deleted after the runs. To re-clone them, use the commands in `questions*.json`.

## Summary (2026-10-30)

| | Firefox @ 0b6146da78 | Chromium `src` @ 0b2cf11831 |
|---|---|---|
| Tree files / C/C++ files indexed | 485,946 / 41,857 | 514,181 / 143,887 |
| Full index | 41 s, 1.0 GB peak RSS | 112 s, 1.1 GB peak RSS |
| DB size | 1.05 GB | 2.47 GB (3.3M symbols, 44.6M refs, 1.44M includes) |
| No-op update | 0.96 s | 1.8 s warm (4.7 s cold) |
| Incremental, 200 changed files | 1.4–1.8 s | 2.1–3.1 s |
| Lookup, best of 5 (process start included) | 18–19 ms | 19–35 ms |
| Index alone vs ground truth (4 questions) | 4/4 exact | 4/4 exact |
| Haiku+index vs Haiku+grep, exact runs | 12/12 vs 12/12 | **12/12 vs 10/12** |
| Haiku tokens, sum of per-question means | **158K vs 337K (−53%)** | **162K vs 351K (−54%)** |

Chromium here is `chromium/src` only. DEPS-pulled repos (V8, Skia and others) are not included.

## Other languages (2026-10-30, index alone vs independent ground truth)

Shallow clones, deleted afterwards. Ground truth was built by `bench/language_truth.py` from
`git grep -P`, Python regexes and path normalization (no tree-sitter), with every discrepancy read in
the source. Re-check with `bench/index_accuracy.py --questions bench/questions-<repo>.json`.

| Language (corpus @ commit) | Files indexed / time | Questions | Result | Lookup |
|---|---|---|---|---|
| Rust (rust-analyzer @ 9e0d1ec24e) | 1,528 / 1.1 s | def struct, trait implementers (37), callers in files (105) | 3/3 exact | 19–20 ms |
| TypeScript (vscode @ 0d958c41dd) | 15,683 / 15 s | def class (3), includers of `glob.ts` (66), callers (682 files), transitive interface implementers (14) | 4/4 exact | 19–21 ms |
| JavaScript (node `lib/` @ 97aed16adb) | 43,772 (mixed) / 24 s | def, `require` importers (178), callers (30) | 3/3 exact | 20 ms |
| C# (efcore @ 716a0a9517) | 5,856 / 4.1 s | def class (3), interface implementers (60), callers (59 files), `using` importers (168) | 4/4 exact | 19–22 ms |
| C++ regression (node `src/`) | (same index) | includers of `env-inl.h` (139), def `Environment` | exact | — |

What the real corpora caught, all fixed with tests:
- TS ESM imports name the emitted file (`./glob.js` for `glob.ts`). The resolver now maps `.js` to `.ts`.
- Rust turbofish method calls (`ctx.find::<T>()`) were missed, at 25 of 100 files. Calls inside macro
  arguments (`vec![f()]`, `assert_eq!(g(x), 1)`) were invisible, because tree-sitter keeps those as
  raw token trees. Both are now recovered.

Instrument errors found while building truth, each resolved by reading the source in the index's
favour: `export`-only class regex; `.tsx` not searched; type arguments containing parentheses or
nested generics; calls inside template or raw strings (not real calls); transitive implementers;
`#include <…>` spellings.

Syntax-error rates are grammar or test-data effects, not extractor bugs:
- rust-analyzer: `crates/parser/test_data` is intentionally broken.
- node: V8's `%Native` test syntax, plus macro-heavy C++ in V8 and OpenSSL.
- efcore: tree-sitter-c-sharp mishandles `async` as a parameter name, which causes local errors.

Hits from files with declaration-level errors are labelled `parse-errors`.

## Questions and ground truth

Ground truth is in `bench/truth/<corpus>/`. It was built with `git grep` / `git grep -P`, plus a Python
multi-line scan for Chromium Q3, and every non-trivial match was read in the source. It was not built
with orch-index.

| | Firefox | Chromium |
|---|---|---|
| Q1 includers | of `netwerk/protocol/http/nsHttpChannel.h`: 24 files | of `net/http/http_response_info.h`: 78 files |
| Q2 definition | class `nsHttpChannel`: `nsHttpChannel.h:63` | class `URLRequestJob`: `url_request_job.h:54` |
| Q3 implementers | `OnStartRequest(nsIRequest*)` under `netwerk/`: 45 classes | `URLRequest::Delegate::OnResponseStarted(URLRequest*, int)`, whole tree, inheritance-aware: 14 classes (4 same-signature `NetworkDelegate` classes excluded) |
| Q4 callers | `NS_GetFinalChannelURI`: 70 calls, 39 files | `IsCertStatusError`: 88 calls, 58 files (2 in `.mm`) |

## Haiku A/B (v2): `bench/haiku_ab.py`

Setup: the orch-explorer role prompt, Haiku at low effort, tools Bash/Read/Grep/Glob, 3 runs per cell,
user settings excluded. The index arm also gets `bench/index-arm.md`. "Tokens" counts input, cache
creation, cache reads and output over all turns; roughly 17K per run is Claude Code's fixed system prompt.

| Q | Firefox grep | Firefox index | Chromium grep | Chromium index |
|---|---|---|---|---|
| q1 includers | 3/3, 47.9K, 64 s | 3/3, **29.0K**, 16 s | 3/3, **30.9K**, 54 s | 3/3, 35.8K, 31 s |
| q2 definition | 3/3, 25.5K, 14 s | 3/3, 26.3K, 13 s | 3/3, 23.9K, 18 s | 3/3, **20.9K**, 15 s |
| q3 implementers | 3/3, 210.4K, 69 s | 3/3, **59.5K**, 26 s | **1/3**, 267.5K, 121 s | 3/3, **73.8K**, 34 s |
| q4 callers | 3/3, 53.5K, 33 s | 3/3, **43.3K**, 22 s | 3/3, **28.7K**, 31 s | 3/3, 31.6K, 22 s |

Reading:
- The index wins clearly on semantic questions. On Q3, with inheritance and same-signature methods in
  unrelated hierarchies, it uses 72% fewer tokens and is about 3× faster on both corpora. On Chromium,
  grep-only Haiku missed 1–2 of the 14 classes in 2 of 3 runs, while the index arm was exact every time.
- On single-search questions it is roughly a tie. Chromium includes and calls are fully qualified, so
  one `git grep -l` / `-nw` already gives a compact, complete answer. There the index arm used 10–16%
  more tokens (its rows carry labels), though it was still faster in wall time.
- The index arm was faster in wall time in every cell.

## What changed between A/B runs (v1 → v2)

The v1 Chromium A/B had the index arm costing more on Q1 (72K vs 31K tokens). Two causes:
- every index-arm agent re-checked with `rg`/`git grep` despite `[resolved]` labels;
- includer rows printed the include line twice, and callers printed 88 rows with snippets.

The fix added a `--files` mode (one row per file), removed the duplicate snippet from includers, and
made "don't re-verify" an explicit rule in `bench/index-arm.md`. Index-arm Q1 tokens fell from 72K to
36K. All index-arm cells were rerun under v2, and the grep arm, which doesn't touch the tool, was
reused. v1 numbers are in the raw directories `ab-run1` and `ab-chromium`.

## Fixes the corpora forced (all have tests or were verified on the real file)

- Firefox: statement-like macros without `;` (`NS_IMPL_ISUPPORTS(...)`, `NS_DECL_ISUPPORTS`)
  mis-scoped the rest of a file. They are now blanked before parsing, keeping byte offsets.
- Chromium: export macros between `class` and the name (`class NET_EXPORT Foo`,
  `COMPONENT_EXPORT(X)`) hid class definitions. Q2 was 0/1 before the fix. Blanking them cut
  Chromium files with syntax errors from 45.3K to 35.2K.
- Chromium: `--base` followed inheritance by last name segment, so `URLRequest::Delegate` matched
  every `Delegate`. It now follows qualified names, with lookup-scope rules. Q3 went from 10 correct,
  3 wrong and 4 missed to 14/14.
- Chromium: a call inside an Objective-C++ region of a `.mm` file was invisible, because error
  recovery builds no call node there. `name(` inside an error node now counts as a call. Verified on
  the real file: 0 call sites found with the rule off, 1 with it on.
- Benchmark tooling: the scorer didn't normalize absolute paths, which marked 3 correct answers wrong
  in the first Firefox pass. `git grep -E` silently ignores `\b`, which gave 0 counts until replaced by `-P`.

## Known limits

- References match by name (`syntactic`). For generic names (`Init`), filter with `--path` or by the
  calling function in the detail.
- 24% of Chromium and 41% of Firefox C/C++ files have at least one tree-sitter error. Most are local
  and harmless. Since 2026-10-30, hits from files with *declaration-level* errors are labelled
  `parse-errors`.
- Objective-C(++) is parsed with the C++ grammar: best effort.
- Cross-language links (JS↔C++ bindings, FFI, IDL) are not modelled. In a mixed repo, name-based
  references can mix languages; filter with `--lang`.
- No compiler-backed (`compiler`) data yet; SCIP ingestion is a later milestone.
