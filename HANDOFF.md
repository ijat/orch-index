# Handoff: start the orch-index project

Written 2026-10-30 by the session that ran the large-repo benchmark. You are starting fresh: plan the
work, then build it. The user wants this done properly for the long term ("we don't cut short here").
Design for repos the size of Chromium's, not just for today's repos.

## Goal

Make huge git repositories (Chromium-class: tens of GB, millions of files, mixed languages)
understandable to orch agents. That means fast, *trustworthy* answers to structural questions:
- where a symbol is defined;
- who calls, includes or imports something;
- which classes override or implement a method;
- what changed, and what it affects.

Answers must be compact and cite `file:line`. Agents must not need to read whole files or dump grep
output.

## The orch projects and how they relate

All of them live under `~/Projects/orch/`:

| Folder | Repo | What it is |
|---|---|---|
| `orch-skill/` | public `github.com/ijat/orch` (MIT) | The orchestration skill: `SKILL.md`, roles → generated `orch-*` agents, the `orch` CLI (`bin/orch`, `bin/orchlib/`), templates, `export.py`, `install.sh`. |
| `orch-pane/` | public `github.com/ijat/orch-pane` | Claude Code plugin: a live side pane showing orchestration status (polls `orch status --json`). |
| `orch-index/` | local only so far (no remote) | **This project**: the indexer. |

How they connect:
- The orch skill runs tasks with roles. `orch-explorer` (Haiku) answers lookups for every other role.
  The indexer exists to back those lookups, through a new Haiku role, probably `orch-graph`, or as a
  tool the explorer calls.
- `orch-index` is a standalone tool: a CLI plus an index store. The orch skill gets only the thin
  integration: a role file, rules for when to call it, and possibly an `orch` subcommand. Keep the
  indexer usable outside orch.
- **Source of truth for the orch skill:** as of 2026-10-30 a task (`orch-skill/repo-as-source`) is
  switching the flow. Afterwards the `orch-skill` repo is the source, and `scripts/sync-live.py` installs
  it into `~/.claude`, adding back a private overlay. Before you touch orch-skill:
  - Check `~/.claude/orchestrations/ACTIVE.md` and that repo's `CLAUDE.md` / README for the current
    workflow. If that task hasn't merged, ask the user.
  - Never run `install.sh` on this machine.
  - Everything in orch-skill is public: no private names, paths or hosts.
- **Agent files are generated.** `agents/orch-*.md` are built from `skills/orchestrate/roles/` by
  `bin/orch-build-agents.py`. Add a new role under `roles/`, never by hand-editing an agent file.

## What we already know (2026-10-30 benchmark)

The full numbers and raw outputs are in `~/.claude/benchmarks/bigrepo-2026-10-30/` (`RESULTS.md`,
`proto/tags2sqlite.py`, `probes/`). The corpus was Firefox, shallow clone: 485,942 files, 4.6 GB working
tree. It has been deleted; re-clone it if you need it (2m39s). Warm-cache numbers on this Mac:

| Thing | Result | Meaning |
|---|---|---|
| Full vs sparse worktree | 49 s / 4.7 GB vs 1.6 s / 29 MB | Already adopted into orch (`checkout: sparse` at ≥ 100K tracked files). |
| `rg` / `git grep`, whole tree | ~10 s and 107 KB per common symbol | Too slow and too much output at Chromium size (estimated, not measured). |
| Same search, one subsystem | 0.07–0.25 s | Scoping works, but impact questions cross subsystems: 8 of 24 includers of one header were outside it. |
| ctags, whole tree | 183 s, 3.3 GB flat file, 12 s per lookup | A flat file does not scale. |
| ctags → SQLite | 50 s load, 1.4 GB, 0.08 s lookup; re-indexing 200 files takes 0.9 s | An indexed store plus incremental updates by changed file works. |
| ctags accuracy | Missed 9/45 `OnStartRequest` implementers (macros, inline `NS_IMETHOD … override {}`), and counted IPC overloads as overrides | Heuristic parsing hits a ceiling on semantic questions. |
| Haiku + scoped grep (no index) | 24/24, 45/45 correct; 17–23K tokens | At Firefox scale, plain tools plus a careful Haiku are already accurate. |
| Haiku + the ctags index | 45/45 correct, but **34K tokens** (more) | An index the agent can't trust *costs* more: it re-verifies everything. **Trustworthiness is the requirement, not speed alone.** |
| claude-repo-context (`netwerk/dns`, 51 files) | 64K tokens; verifier clean; summaries shallow; 70% of include edges unresolved (`mozilla/…` headers mapped by the build) | Per-file LLM summaries don't scale (~1.25K tokens/file, about 450M for all of Firefox). Include resolution needs build-system knowledge. |

Prior art (checked 2026-10-30):
- Chromium code search's cross-references come from **Kythe**, built from a compilation database.
- Firefox **Searchfox** uses a Clang plugin that runs during a full build.
- **scip-clang** (Sourcegraph, beta, Clang 16+) needs `compile_commands.json`, about 2 MB of temp
  space per translation unit, and 30–50% more time than type-checking.
- **github/stack-graphs** was archived on 2025-09-09.

Conclusion: every compiler-accurate C++ indexer at this scale needs a build. A build-free layer can only
be heuristic, so it must label its confidence.

Gotchas we hit:
- `rg` with no path argument and a non-terminal stdin waits on stdin forever (0% CPU). Always pass a
  path and `</dev/null`.
- `git grep -E` doesn't support `\s` (POSIX ERE), and that silently broke a ground-truth regex. Check
  every instrument on a known case first.
- Haiku at low effort ignores conditional rules buried in bullets. Write must-do rules as an explicit
  first command.

## Proposed direction (a starting point; you plan it)

- **Rust**, one binary with library crates.
- **Layer 1, build-free, every language:** tree-sitter parsing for definitions, references, imports and
  includes, and class hierarchy where it can be derived syntactically. Every fact carries a confidence or
  provenance (`syntactic`, `resolved`, `compiler`).
- **Layer 2, precise when a build exists:** ingest SCIP (scip-clang, rust-analyzer, scip-typescript,
  and so on) and possibly Kythe or compile_commands-driven data. Precise facts override heuristic ones.
- **Build-system awareness** for include and import resolution: compile_commands, moz.build EXPORTS, GN,
  Bazel, CMake. This is the 70%-unresolved problem.
- **Storage:** sharded SQLite, or another embedded store if measurements justify it. Indexed by symbol,
  file and edge type. It needs to work at Chromium size on a laptop.
- **Incremental updates:** keyed on git SHAs (`git diff --name-only <indexed>..HEAD`), re-indexing changed
  files only. It must work with sparse worktrees: index the committed tree (`HEAD`), not only checked-out
  files.
- **Query CLI** for agents: tiny output by default, `file:line` plus a one-line snippet, counts first,
  JSON on request. Example queries: `def`, `refs`, `callers`, `includers`, `overrides`, `impact <diff>`.
- **orch integration:** an `orch-graph` Haiku role, or explorer rules saying "query the index first and
  fall back to `git grep` when its confidence is low".

## How to work

- Use orchestration mode (the `orchestrate` skill). This is a program-sized job: ask for, or propose,
  **program mode** (milestones). The user has pre-approved orchestration without being asked first.
- **Benchmark on real repos with ground truth.**
  - Firefox is the mid-size corpus. Chromium is the target. The user said not to clone Chromium
    casually: it's a monster, so ask before you do. Consider a partial or shallow clone, or a large
    subset.
  - Clone into a temporary directory and **delete it afterwards**.
  - Keep results in a durable dir such as `~/.claude/benchmarks/` (the session scratchpad is wiped on
    restart).
  - Build ground truth independently and verify it by reading source. Our first truth set was wrong.
- **Token budget:**
  - Test with Haiku agents. If a stronger model would help a test, say so and ask first.
  - Orch's own planning, implementing and reviewing stages use their normal tiers.
  - Check `~/.claude/bin/usage-check.sh` before long runs.
- Tooling notes: in Rust, pin the toolchain. For any JS, use pnpm, never npm. Never run Playwright WebKit
  natively on macOS.
- Decide early whether `orch-index` becomes a public repo. If it does, keep private names, paths and
  hosts out of it from the first commit.

## Done means

1. On Firefox and on a Chromium-scale corpus, measured against independent ground truth:
   - correct answers to definition, references/callers, includers, and overrides/implementers questions;
   - honest confidence labels where precise data is missing;
   - lookups in well under a second, with incremental updates measured.
2. A Haiku agent using the index beats Haiku with grep on tokens **and** accuracy, on the same questions.
   That's the bar the ctags index failed.
3. orch integration is merged through orch-skill's workflow (role, rules, docs, probes old vs new).
4. Reproducible benchmark scripts and results are committed in this repo.
