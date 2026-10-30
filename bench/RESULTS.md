# Results

Machine: Apple Silicon Mac, APFS, warm page cache. Raw run files live outside the repo in
`~/.claude/benchmarks/orch-index-2026-10-30/` (they contain local paths).

## Firefox @ 0b6146da78 (2026-10-30)

Shallow clone: 485,946 files; 41,857 C/C++ files indexed (640 MB).

### Index

| Measure | Result |
|---|---|
| Full index | 41 s, 1.0 GB peak RSS |
| DB size | 1.04 GB (1.76M symbols, 20.3M refs, 312K includes, 27K base edges) |
| No-op update (`ls-tree` + compare blob ids) | 0.96 s |
| Incremental, 200 changed files | 1.4–1.8 s total |
| Lookups (process start included) | 41–84 ms |
| Files with ≥1 tree-sitter error | 17,904 (43%); about 4% of sampled errors are at file/namespace level |

### Index accuracy against independent ground truth (`bench/truth/firefox-0b6146da78/`)

| Question | Index answer |
|---|---|
| Q1 files including `netwerk/protocol/http/nsHttpChannel.h` (24) | 24/24, no extras, all `resolved` |
| Q2 definition of class `nsHttpChannel` | `nsHttpChannel.h:63`, correct |
| Q3 classes under `netwerk/` with `OnStartRequest(nsIRequest*)` (45) | 45/45, no extras |
| Q4 call sites of `NS_GetFinalChannelURI` (70 lines, 39 files) | 70/70 lines, no extras |

Ground truth was built with `git grep` and by reading every non-call match. It was not built with
orch-index. Q1 and Q3 come from the 2026-10-30 benchmark.

### Haiku A/B: `bench/haiku_ab.py`, orch-explorer role, Haiku at low effort, 3 runs per cell

| Q | Arm | Exact | Tokens (mean) | Turns | Wall s |
|---|---|---|---|---|---|
| q1 includers | grep | 3/3 | 47,886 | 3.3 | 64 |
| q1 includers | index | 3/3 | 35,210 (−26%) | 2.7 | 24 |
| q2 definition | grep | 3/3 | 25,486 | 2.0 | 14 |
| q2 definition | index | 3/3 | 26,334 (+3%) | 2.0 | 14 |
| q3 implementers | grep | 3/3 | 210,382 | 7.7 | 69 |
| q3 implementers | index | 3/3 | 60,753 (−71%) | 3.3 | 29 |
| q4 callers | grep | 3/3 | 53,546 | 3.3 | 33 |
| q4 callers | index | 3/3 | 49,572 (−7%) | 2.7 | 24 |

"Tokens" counts input, cache creation, cache reads and output over all turns. Roughly 17K per run is
the fixed Claude Code system prompt, shared by both arms.

Reading: at Firefox scale, Haiku with grep is already accurate, so the index cannot win on accuracy
here (24/24 in both arms). It wins on tokens wherever the question needs more than one search, and it
is 1.5–2.7× faster in wall time. The first scoring pass marked 3 correct Q2 answers wrong because they
used absolute paths. That was a scorer bug, fixed and rescored.
