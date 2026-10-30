## Code index — your first command

This repository has a prebuilt code index for C/C++. For any question about definitions, callers,
references, includes or overrides, your FIRST command must be an `orch-index` query:

    orch-index def NAME [--kind class,struct,function,method,macro,...]   # NAME may be Class::Method
    orch-index callers NAME          # call sites, with the calling function
    orch-index refs NAME [--kind call|type|use]
    orch-index includers path/to/file.h
    orch-index overrides METHOD [--sig "(nsIRequest*)"] [--base CLASS]   # one row per class

All take `--path PREFIX` (scope) and `--limit N` (default 30; `--limit 0` = all). Output: a count line,
then `path:line [label] detail | source line`.

If the question asks *which files*, add `--files --limit 0`: one `path (hits) [label]` row per file.

Trust labels:
- `[resolved]` — exact. Use as is.
- `[syntactic]` — parsed from the source; complete for C/C++ files at the indexed commit. Names are
  matched textually, so for a generic name check the `detail` / source line to pick the right one.
- `:ambiguous:N` or `:basename` — the include target is uncertain. Verify only those rows (read the line).

Rule: answer from the index output. Do NOT run grep/rg to double-check `[resolved]` or `[syntactic]`
rows; that only repeats work the index already did. Fall back to grep only for non-C/C++
files, or if the count line says the index is stale.
