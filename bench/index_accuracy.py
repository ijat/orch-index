#!/usr/bin/env python3
"""Scores orch-index's own answers (no agent) against the ground truth.

  bench/index_accuracy.py --corpus DIR [--questions FILE]
"""
import argparse
import json
import subprocess
import time
from pathlib import Path

BENCH = Path(__file__).resolve().parent
BIN = BENCH.parent / "target/release/orch-index"


def items(q, hits):
    if q["unit"] == "path":
        return {h["path"] for h in hits}
    if q["unit"] == "path:line":
        return {f"{h['path']}:{h['line']}" for h in hits}
    # class: the detail is "<qualified scope> <signature>"
    return {h["detail"].split(" ")[0].split("::")[-1] for h in hits}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--questions", default=str(BENCH / "questions.json"))
    args = ap.parse_args()
    spec = json.loads(Path(args.questions).read_text())
    truth_dir = BENCH / "truth" / spec["truth_dir"]
    print(f"| q | command | truth | hits | missed | extra | best of 5, ms |\n|---|---|---|---|---|---|---|")
    for q in spec["questions"]:
        cmd = [str(BIN), "--repo", args.corpus, *q["index_cmd"], "--json", "--limit", "0", "--no-snippet"]
        times = []
        for _ in range(5):
            start = time.time()
            out = json.loads(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout)
            times.append((time.time() - start) * 1000)
        ms = min(times)
        truth = {l.strip() for l in (truth_dir / q["truth"]).read_text().splitlines() if l.strip()}
        got = items(q, out["hits"])
        print(f"| {q['id']} | `{' '.join(q['index_cmd'])}` | {len(truth)} | {len(got)} | "
              f"{', '.join(sorted(truth - got)) or '-'} | {', '.join(sorted(got - truth)) or '-'} | {ms:.0f} |")


if __name__ == "__main__":
    main()
