"""Usage: language_truth.py OUT_DIR CORPORA_DIR

Ground truth for the language validation corpora, built with git grep + regex (no tree-sitter),
then compared with orch-index. Prints truth size, misses and extras per question."""
import json, os, re, subprocess, sys

C = sys.argv[2]  # dir holding the four clones
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "orch-index")
OUT = sys.argv[1]  # dir for truth files


def grep(repo, pat, *paths, flags=("-n",)):
    r = subprocess.run(["git", "-C", f"{C}/{repo}", "grep", "-P", *flags, pat, "--", *paths],
                       capture_output=True, text=True, stdin=subprocess.DEVNULL)
    return [l for l in r.stdout.splitlines() if l]


def read(repo, path):
    return open(f"{C}/{repo}/{path}", errors="replace").read()


def index(repo, *args):
    r = subprocess.run([BIN, "--repo", f"{C}/{repo}", *args, "--json", "--limit", "0", "--no-snippet"],
                       capture_output=True, text=True, check=True)
    return json.loads(r.stdout)["hits"]


results = []


def compare(qid, repo, truth, got, cmd, unit):
    truth, got = set(truth), set(got)
    os.makedirs(f"{OUT}/{repo}", exist_ok=True)
    open(f"{OUT}/{repo}/{qid}.txt", "w").write("".join(f"{t}\n" for t in sorted(truth)))
    results.append({"repo": repo, "id": qid, "truth": f"{qid}.txt", "unit": unit, "index_cmd": cmd})
    print(f"{repo} {qid}: truth {len(truth)} got {len(got)} | missed {sorted(truth - got)[:12]} | extra {sorted(got - truth)[:12]}")


def classes(hits):
    return {h["detail"].split(" ")[0].split("::")[-1] for h in hits}


# ---------------- Rust: rust-analyzer ----------------
R = "rust-analyzer"
t = {f"{l.split(':')[0]}:{l.split(':')[1]}" for l in grep(R, r"^\s*pub struct Semantics\b", "*.rs")}
cmd = ["def", "Semantics", "--kind", "struct"]
compare("def-semantics", R, t, {f"{h['path']}:{h['line']}" for h in index(R, *cmd)}, cmd, "path:line")

# Types implementing HirDisplay (generic trait), one line per impl.
impl_re = re.compile(r"\bimpl\b(?:<[^{]*?>)?\s+(?:[\w:]+::)?HirDisplay\b(?:<[^>]*>)?\s+for\s+&?(?:[\w:]+::)?(\w+)")
t = set()
for l in grep(R, r"\bHirDisplay\b(<[^>]*>)?\s+for\b", "*.rs"):
    m = impl_re.search(l)
    if m:
        t.add(m.group(1))
cmd = ["overrides", "hir_fmt", "--base", "HirDisplay"]
compare("impl-hirdisplay", R, t, classes(index(R, *cmd)), cmd, "class")

call_re = r"\bfind_node_at_offset\s*(::<.*>)?\("
t = set()
for l in grep(R, call_re, "*.rs"):
    path, line, text = l.split(":", 2)
    if re.search(r"\bfn\s+find_node_at_offset\b", text) or text.strip().startswith("//"):
        continue
    t.add(path)
cmd = ["callers", "find_node_at_offset", "--lang", "rust"]
compare("callers-find-node", R, t, {h["path"] for h in index(R, *cmd)}, cmd, "path")

# ---------------- TypeScript: vscode ----------------
V = "vscode"
t = {f"{l.split(':')[0]}:{l.split(':')[1]}" for l in grep(V, r"^(export\s+)?(abstract\s+)?class\s+TextModel\b", "*.ts", "*.tsx")}
cmd = ["def", "TextModel", "--kind", "class"]
compare("def-textmodel", V, t, {f"{h['path']}:{h['line']}" for h in index(V, *cmd)}, cmd, "path:line")

target = "src/vs/base/common/glob.ts"
t = set()
for l in grep(V, r"""(from|import)\s*\(?\s*['"][./][^'"]*glob(\.js)?['"]""", "*.ts", "*.tsx"):
    path, line, text = l.split(":", 2)
    m = re.search(r"""['"]([./][^'"]*)['"]""", text)
    resolved = os.path.normpath(os.path.join(os.path.dirname(path), m.group(1)))
    resolved = re.sub(r"\.js$", "", resolved)
    if resolved + ".ts" == target:
        t.add(path)
cmd = ["includers", target, "--files"]
compare("includers-glob", V, t, {h["path"] for h in index(V, "includers", target)}, cmd, "path")

call_re = r"\bcreateDecorator\s*(<.*>)?\s*\("
t = set()
for l in grep(V, call_re, "*.ts", "*.tsx"):
    path, line, text = l.split(":", 2)
    if re.search(r"\bfunction\s+createDecorator\b", text) or text.strip().startswith(("//", "*")):
        continue
    t.add(path)
# Adjudicated by reading: line 60 is inside a template-string test fixture, not a call.
t.discard("build/next/test/bundle.test.ts")
cmd = ["callers", "createDecorator", "--lang", "typescript"]
compare("callers-createdecorator", V, t, {h["path"] for h in index(V, *cmd)}, cmd, "path")

# Transitive implementers that define provide(): class graph from regexes over every .ts file.
decl = re.compile(r"\bclass\s+(\w+)\s*(?:<[^{]*?>)?\s*(?:extends\s+([\w.]+)\s*(?:<[^{]*?>)?)?\s*(?:implements\s+([^{]+))?\{")
parents, defines = {}, set()
for path in grep(V, r"\bclass\s+\w+", "*.ts", flags=("-l",)):
    src = read(V, path)
    for m in decl.finditer(src):
        name = m.group(1)
        ps = set()
        if m.group(2):
            ps.add(m.group(2).split(".")[-1])
        if m.group(3):
            ps |= {x.strip().split("<")[0].split(".")[-1] for x in m.group(3).split(",")}
        parents.setdefault(name, set()).update(ps)
        depth, i = 1, m.end()
        while i < len(src) and depth:
            depth += {"{": 1, "}": -1}.get(src[i], 0)
            i += 1
        body = src[m.end():i]
        if re.search(r"^\s*(public\s+|protected\s+|override\s+|async\s+)*provide\s*(<[^>]*>)?\s*\(", body, re.M):
            defines.add(name)
closure, frontier = set(), {"IQuickAccessProvider"}
while frontier:
    nxt = {c for c, ps in parents.items() if ps & frontier} - closure
    closure |= nxt
    frontier = nxt
t = closure & defines
cmd = ["overrides", "provide", "--base", "IQuickAccessProvider"]
compare("impl-quickaccess", V, t, classes(index(V, *cmd)), cmd, "class")

# ---------------- JavaScript: node lib/ ----------------
N = "node"
t = {f"{l.split(':')[0]}:{l.split(':')[1]}" for l in grep(N, r"^(const|let|var|function)\s+validateString\b", "lib/*.js")}
cmd = ["def", "validateString", "--lang", "javascript", "--path", "lib/"]
compare("def-validatestring", N, t, {f"{h['path']}:{h['line']}" for h in index(N, *cmd)}, cmd, "path:line")

t = {l for l in grep(N, r"""require\(\s*['"]internal/validators['"]\s*\)""", "lib/*.js", flags=("-l",))}
cmd = ["importers", "internal/validators", "--path", "lib/"]
compare("importers-validators", N, t, {h["path"] for h in index(N, *cmd)}, cmd, "path")

t = set()
for l in grep(N, r"\bemitExperimentalWarning\s*\(", "lib/*.js"):
    path, line, text = l.split(":", 2)
    if re.search(r"\bfunction\s+emitExperimentalWarning\b", text) or text.strip().startswith(("//", "*")):
        continue
    t.add(path)
cmd = ["callers", "emitExperimentalWarning", "--path", "lib/"]
compare("callers-experimental", N, t, {h["path"] for h in index(N, *cmd)}, cmd, "path")

# ---------------- C#: efcore ----------------
E = "efcore"
t = {f"{l.split(':')[0]}:{l.split(':')[1]}" for l in grep(E, r"^\s*public\s+(partial\s+)?class\s+DbContext\b", "*.cs")}
cmd = ["def", "DbContext", "--kind", "class"]
compare("def-dbcontext", E, t, {f"{h['path']}:{h['line']}" for h in index(E, *cmd)}, cmd, "path:line")

# Direct implementers of IMethodCallTranslator (base list may span lines: read whole files).
t = set()
for path in grep(E, r"\bIMethodCallTranslator\b", "*.cs", flags=("-l",)):
    src = read(E, path)
    for m in re.finditer(r"\bclass\s+(\w+)(?:<[^>{]*>)?(?:\([^)]*\))?\s*:\s*([^{]*)\{", src):
        if re.search(r"\bIMethodCallTranslator\b", m.group(2)):
            t.add(m.group(1))
cmd = ["overrides", "Translate", "--base", "IMethodCallTranslator"]
compare("impl-methodcalltranslator", E, t, classes(index(E, *cmd)), cmd, "class")

def mask_cs(src):
    """Blank out comments and string literals (raw, verbatim, regular), keeping line breaks."""
    pat = re.compile(r'\$*"{3,}[\s\S]*?"{3,}|\$?@"(?:[^"]|"")*"|\$?"(?:\\.|[^"\\\n])*"|//[^\n]*|/\*[\s\S]*?\*/')
    return pat.sub(lambda m: re.sub(r"[^\n]", " ", m.group(0)), src)
t = set()
for path in grep(E, r"\bUseSqlServer\b", "*.cs", flags=("-l",)):
    src = mask_cs(read(E, path))
    for m in re.finditer(r"\bUseSqlServer\s*(<[^()]*>)?\s*\(", src):
        head = src[src.rfind("\n", 0, m.start()) + 1 : m.start()]
        if re.search(r"\b(public|internal|private|protected)\b", head):
            continue  # a definition
        t.add(path)
        break
cmd = ["callers", "UseSqlServer"]
compare("callers-usesqlserver", E, t, {h["path"] for h in index(E, *cmd)}, cmd, "path")

ns = "Microsoft.EntityFrameworkCore.Query.SqlExpressions"
t = {l for l in grep(E, r"^\s*(global\s+)?using\s+Microsoft\.EntityFrameworkCore\.Query\.SqlExpressions\s*;", "*.cs", flags=("-l",))}
cmd = ["importers", ns]
compare("importers-sqlexpressions", E, t, {h["path"] for h in index(E, *cmd)}, cmd, "path")

json.dump(results, open(f"{OUT}/questions.json", "w"), indent=1)
