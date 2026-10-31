# Requirement: `orch-index status --json`

Requested by the `orch-pane` project (`../orch-pane`), which shows index health in a collapsed "Index"
section under each task's Goal. The pane polls this command every 8 s, so it must be cheap and
side-effect free. Implement it here; the pane side is built afterwards against this contract.

## Command

```
orch-index status --json [--repo DIR] [--db FILE]
```

The pane runs it with cwd set to the session folder, as it does `orch status --json --cwd`.

## Behaviour

- Opens the db **read-only and never creates it**. (Today `stats` creates an empty `index.db` when
  none exists; the pane would be polling this.)
- No tree walk. Target: well under 200 ms.
- A missing index is not an error: exit 0 with `"state":"none"`. Exit non-zero only when the folder is
  not a git repository.
- One JSON object on stdout. All strings sanitized like the other output (control characters escaped).
- Logs nothing to the usage tables (see below), so polling cannot inflate the counts.

## Output (schema 1)

Unknown or unavailable values are `null`. Everything marked *optional* may be absent; the pane shows a
field only when present.

```json
{
  "schema": 1,
  "state": "none | current | stale | outdated",
  "indexing": false,
  "repo": "/abs/toplevel",
  "db": "/abs/.git/orch-index/index.db",
  "db_bytes": 123456789,
  "indexed_commit": "sha",
  "head_commit": "sha",
  "commits_behind": 12,
  "indexed_at": "2026-10-31T01:27:00Z",
  "extractor_version": "0.3.1",
  "binary_extractor_version": "0.3.2",
  "counts": {"files": 0, "symbols": 0, "refs": 0, "includes": 0, "bases": 0, "names": 0},
  "languages": {"cpp": 0, "rust": 0},
  "parse_error_files": 0,
  "parse_error_structural": 0,
  "last_index": {
    "kind": "full | incremental",
    "seconds": 12.4, "list_seconds": 0.8, "extract_seconds": 11.6,
    "files_total": 485942, "extracted": 200, "reused": 485742, "skipped_large": 3,
    "files_per_sec": 16.1,
    "db_bytes_before": 1400000000, "db_bytes_after": 1400200000
  },
  "history": [{"at": "2026-10-31T01:27:00Z", "kind": "incremental", "seconds": 0.9, "extracted": 200}],
  "usage": {
    "window_days": 7, "queries": 412,
    "by_command": {"def": {"n": 200, "p50_ms": 4, "p95_ms": 31}, "callers": {"n": 90, "p50_ms": 9, "p95_ms": 80}},
    "zero_hit_pct": 8.0,
    "stale_answer_pct": 2.0,
    "label_mix": {"resolved": 0.31, "syntactic": 0.64, "ambiguous": 0.04, "parse_errors": 0.01},
    "last_query_at": "2026-10-31T01:30:00Z"
  }
}
```

**Required:** `schema`, `state`, `indexing`, `indexed_commit`, `head_commit`, `commits_behind`,
`indexed_at`, `db_bytes`, `counts`, `parse_error_files`, `parse_error_structural`.
**Optional:** `languages`, `last_index`, `history`, `usage`, `extractor_version`,
`binary_extractor_version`, `repo`, `db`.

## Meaning of `state`

- `none`: no index.
- `current`: `indexed_commit == head_commit` and the extractor version matches.
- `stale`: the commits differ. `commits_behind` = `git rev-list --count indexed..HEAD`, or `null` when
  `indexed` is not an ancestor of HEAD (rebase, branch switch).
- `outdated`: built by a different extractor version than this binary's.
- Both stale and outdated → `outdated` (only a re-index fixes it).

## Data to start recording

At the end of each `index` run, write to `meta`:
- `indexed_at`: finish time of the run (not the commit date).
- `extractor_version`, if not already stored.
- `last_index`: the block above, plus a bounded `history` of the last ~10 runs (newest first).

`indexing` is true while the index lock is held.

## Usage aggregates (query performance and analytics)

`orch-index` aggregates its own queries into a `usage` table: one row per **day and command**
(`def`, `callers`, `includers`, `importers`, `overrides`, `refs`). Keep 30 days. Never store query text
or paths. Per row:

- `count`, `total_ms`, `max_ms`
- latency histogram buckets (so p50/p95 can be derived)
- `hits_total`, `zero_hit` (queries with no hits)
- `stale_answers`: queries answered while `state != current`
- `labels`: counts per trust label (`resolved`, `syntactic`, `syntactic:ambiguous`, `…:parse-errors`)

`status --json` rolls these up over the last 7 days into the `usage` block above.

Constraints:
- Best-effort: a failed or locked write must never fail a query nor add more than ~2 ms.
- Write only the aggregate row, never one row per query.
- `ORCH_INDEX_NO_USAGE=1` turns it off (then `usage` is absent).
- Put the new tables under a schema bump that does **not** trigger the extractor-version "outdated"
  check (an old index must not become `outdated` just because these tables are new).
- `status` is read-only and logs nothing.

## Why

The index pays off only when agents trust it (HANDOFF: an untrusted index costs more tokens, because
the agent re-verifies everything). `stale_answer_pct`, `zero_hit_pct` and `label_mix` show directly
whether agents are getting trusted answers.

## Install

The pane looks for the binary at `~/.claude/bin/orch-index`, where `orch` lives. If it is missing the
pane shows "orch-index not installed" and nothing else breaks. Tell the pane's session where the
binary ends up if it differs.

## Pane side (for review; built afterwards)

- A collapsed "Index" row under Goal: `▸ Index · current · 1.4 GB · 3m ago`, coloured by state.
  A warning badge when `stale_answer_pct` > 10%.
- Expanded: indexed vs HEAD commit and commits behind; counts; parse errors; languages; last run
  (kind, duration, files/s, extracted vs reused); run-time sparkline from `history`; 7-day queries with
  p50/p95 per command; zero-hit %, stale-answer %, trust-label mix bar; the `orch-index index`
  command when not current.
