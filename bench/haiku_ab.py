#!/usr/bin/env python3
"""A/B: Haiku (orch-explorer role) answering structural questions with grep only vs with orch-index.

Usage:
  bench/haiku_ab.py --corpus DIR --bin DIR --out DIR [--questions FILE] [--reps N] [--jobs N] [--only q1,q3] [--arms grep,index]
  bench/haiku_ab.py --score-only --out DIR

--corpus  the corpus checkout, already indexed (`orch-index index` run inside it)
--bin     directory holding the `orch-index` binary (put on PATH for the index arm only)
--out     results directory; one raw JSON per run plus summary.md

Run it only on trusted corpora: the agents get Bash pre-approved inside the checkout, and a
corpus shipping .claude/, CLAUDE.md or .mcp.json is refused.

Both arms get the same model, effort, tools, role prompt and question. The index arm additionally
gets bench/index-arm.md appended to its system prompt. User settings are excluded
(--setting-sources project) so local hooks cannot alter prompts.
"""

import argparse
import concurrent.futures as cf
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

BENCH = Path(__file__).resolve().parent
ROLE = Path.home() / ".claude/agents/orch-explorer.md"
TOOLS = "Bash,Read,Grep,Glob"


def role_prompt() -> str:
    text = ROLE.read_text()
    m = re.match(r"^---\n.*?\n---\n(.*)$", text, re.S)
    return m.group(1) if m else text


def check_corpus(corpus):
    """The agents get Bash pre-approved inside the corpus. Refuse corpora that ship their own
    Claude Code configuration (hooks, settings, instructions): only run on trusted checkouts."""
    for name in (".claude", "CLAUDE.md", ".mcp.json"):
        if (Path(corpus) / name).exists():
            sys.exit(f"refusing: {corpus}/{name} exists; this harness runs agents with Bash in the corpus")


def run_one(args, q, arm, rep, answer_format):
    out = Path(args.out) / f"{q['id']}-{arm}-{rep}.json"
    if out.exists():
        return out
    system = role_prompt()
    env = dict(os.environ)
    if arm == "index":
        system += "\n\n" + (BENCH / "index-arm.md").read_text()
        env["PATH"] = f"{args.bin}:{env['PATH']}"
    cmd = [
        "claude", "-p", f"{q['prompt']}\n\n{answer_format}",
        "--model", "haiku", "--effort", "low",
        "--output-format", "json",
        "--tools", TOOLS, "--allowedTools", TOOLS,
        "--append-system-prompt", system,
        "--setting-sources", "project",
        "--strict-mcp-config", "--no-session-persistence",
    ]
    start = time.time()
    proc = subprocess.run(cmd, cwd=args.corpus, env=env, capture_output=True, text=True,
                          stdin=subprocess.DEVNULL, timeout=900)
    try:
        data = json.loads(proc.stdout)
    except json.JSONDecodeError:
        data = {"error": proc.stderr[-2000:], "stdout": proc.stdout[-2000:]}
    data["wall_s"] = round(time.time() - start, 1)
    data["cwd"] = str(Path(args.corpus).resolve())
    out.write_text(json.dumps(data))
    return out


def answer_items(result: str):
    blocks = re.findall(r"```answer\s*\n(.*?)```", result or "", re.S)
    if not blocks:
        return None
    return [l.strip().strip("`").strip() for l in blocks[-1].splitlines() if l.strip()]


def normalize(item: str, unit: str, root: str = "") -> str:
    item = re.sub(r"^[-*\d.)\s]+", "", item).strip()
    # Agents sometimes answer with absolute paths; make them repository-relative.
    if root and item.startswith(root.rstrip("/") + "/"):
        item = item[len(root.rstrip("/")) + 1:]
    if unit == "path":
        return item.split(":")[0].lstrip("./")
    if unit == "class":
        return item.split("(")[0].split("::")[-1].strip()
    return item.lstrip("./")


def score(q, items, truth_dir, root=""):
    truth = {l.strip() for l in (truth_dir / q["truth"]).read_text().splitlines() if l.strip()}
    if items is None:
        return {"tp": 0, "fp": 0, "fn": len(truth), "exact": False, "parsed": False}
    got = {normalize(i, q["unit"], root) for i in items}
    if q["unit"] == "path:line":
        # A definition answer is right if it names the right file and line (±1 for attribute lines).
        def ok(g):
            return any(g.split(":")[0] == t.split(":")[0] and g.split(":")[-1].isdigit()
                       and abs(int(g.split(":")[-1]) - int(t.split(":")[1])) <= 1 for t in truth)
        tp = sum(1 for g in got if ok(g))
        return {"tp": tp, "fp": len(got) - tp, "fn": len(truth) - min(tp, len(truth)),
                "exact": tp == len(truth) and len(got) == tp, "parsed": True}
    tp = len(got & truth)
    return {"tp": tp, "fp": len(got - truth), "fn": len(truth - got),
            "exact": got == truth, "parsed": True,
            "missed": sorted(truth - got)[:10], "extra": sorted(got - truth)[:10]}


def tokens(data):
    u = data.get("usage") or {}
    return sum(u.get(k, 0) or 0 for k in ("input_tokens", "cache_creation_input_tokens",
                                           "cache_read_input_tokens", "output_tokens"))


def summarize(args, spec):
    truth_dir = BENCH / "truth" / spec["truth_dir"]
    rows = []
    for f in sorted(Path(args.out).glob("q*-*-*.json")):
        qid, arm, rep = f.stem.split("-")
        q = next(x for x in spec["questions"] if x["id"] == qid)
        data = json.loads(f.read_text())
        s = score(q, answer_items(data.get("result", "")), truth_dir, data.get("cwd") or args.corpus or "")
        rows.append({"q": qid, "arm": arm, "rep": rep, **s, "tokens": tokens(data),
                     "out_tokens": (data.get("usage") or {}).get("output_tokens", 0),
                     "cost": data.get("total_cost_usd", 0), "turns": data.get("num_turns", 0),
                     "wall": data.get("wall_s", 0), "error": "error" in data})
    (Path(args.out) / "rows.json").write_text(json.dumps(rows, indent=1))

    lines = ["| q | arm | runs | exact | recall | precision | tokens (mean) | output tok | turns | cost $ | wall s |",
             "|---|---|---|---|---|---|---|---|---|---|---|"]
    for qid in sorted({r["q"] for r in rows}):
        for arm in ("grep", "index"):
            rs = [r for r in rows if r["q"] == qid and r["arm"] == arm]
            if not rs:
                continue
            n = len(rs)
            tp = sum(r["tp"] for r in rs); fp = sum(r["fp"] for r in rs); fn = sum(r["fn"] for r in rs)
            mean = lambda k: sum(r[k] for r in rs) / n  # noqa: E731
            lines.append(
                f"| {qid} | {arm} | {n} | {sum(r['exact'] for r in rs)}/{n} | "
                f"{tp / max(tp + fn, 1):.2f} | {tp / max(tp + fp, 1):.2f} | {mean('tokens'):,.0f} | "
                f"{mean('out_tokens'):,.0f} | {mean('turns'):.1f} | {mean('cost'):.3f} | {mean('wall'):.0f} |")
    for r in rows:
        if not r["exact"] and (r.get("missed") or r.get("extra") or not r["parsed"]):
            lines.append(f"\n- {r['q']}-{r['arm']}-{r['rep']}: parsed={r['parsed']} "
                         f"missed={r.get('missed')} extra={r.get('extra')}")
    text = "\n".join(lines) + "\n"
    (Path(args.out) / "summary.md").write_text(text)
    print(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus")
    ap.add_argument("--bin")
    ap.add_argument("--out", required=True)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--only", default="")
    ap.add_argument("--arms", default="grep,index")
    ap.add_argument("--score-only", action="store_true")
    ap.add_argument("--questions", default=str(BENCH / "questions.json"))
    args = ap.parse_args()
    spec = json.loads(Path(args.questions).read_text())
    Path(args.out).mkdir(parents=True, exist_ok=True)
    if not args.score_only:
        if not (args.corpus and args.bin):
            sys.exit("--corpus and --bin are required unless --score-only")
        check_corpus(args.corpus)
        qs = [q for q in spec["questions"] if not args.only or q["id"] in args.only.split(",")]
        jobs = [(q, arm, rep) for rep in range(1, args.reps + 1) for q in qs
                for arm in args.arms.split(",")]
        with cf.ThreadPoolExecutor(args.jobs) as pool:
            futs = [pool.submit(run_one, args, q, arm, rep, spec["answer_format"]) for q, arm, rep in jobs]
            for f in cf.as_completed(futs):
                print("done", f.result().name, flush=True)
    summarize(args, spec)


if __name__ == "__main__":
    main()
