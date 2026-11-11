"""gatetrain lanes: a pre-stack collision radar. Read-only advice; shells out to git only."""
import fnmatch
import json
import re
import subprocess
import sys

DEFAULT_SHARED = ["package.json", "package-lock.json", "*.lock", "*.csv", "docs/*.md", "*routes*.json", "*registry*.json"]
DEFAULT_RESOURCE = {"port": r"\b(5[0-9]{4})\b"}
PAIRED_LINE = re.compile(r"EXPECTED|expected exactly|const [A-Z_]+ = \d+")
RANK = {"SAME-LINES": 0, "DELETE-EDIT": 0, "CODE-BOTH": 1, "SHARED-HUB": 2}
ACTION = {
    "SAME-LINES": "certain conflict, merge by hand",
    "DELETE-EDIT": "one lane deletes or renames it, the other edits it: decide which wins, by hand",
    "CODE-BOTH": "a person reads both diffs",
    "SHARED-HUB": "rebuild, never text-merge; check entries survive",
}


class GitError(Exception):
    pass


class Git:
    def __init__(self, repo):
        self.repo = repo

    def run(self, *args, ok=(0,)):
        try:
            r = subprocess.run(["git", "-c", "core.quotepath=off", *args], cwd=self.repo, capture_output=True,
                               text=True, encoding="utf-8", errors="replace")
        except OSError as e:
            raise GitError(str(e))
        if r.returncode not in ok:
            raise GitError(f"git {' '.join(args)}: {r.stderr.strip()}")
        return r.stdout, r.returncode

    def out(self, *args):
        return self.run(*args)[0].strip()


def parse_diff(text):
    """-> {path: [hunk]} ; hunk = dict(old_start, old_count, removed[], added[])"""
    files, cur, hunk = {}, None, None
    for line in text.split("\n"):
        if line.startswith("diff --git "):
            cur, hunk = None, None
        elif line.startswith("--- ") and hunk is None:
            cur = line[4:].split("\t")[0]
        elif line.startswith("+++ ") and hunk is None:
            p = line[4:].split("\t")[0]
            if p == "/dev/null":
                p = cur
            p = p[2:] if p and p[:2] in ("a/", "b/") else p
            cur = p
            files.setdefault(cur, [])
        elif line.startswith("@@") and cur is not None:
            m = re.match(r"@@ -(\d+)(?:,(\d+))? \+\d+(?:,\d+)? @@", line)
            hunk = {"old_start": int(m.group(1)), "old_count": int(m.group(2)) if m.group(2) is not None else 1,
                    "removed": [], "added": []}
            files[cur].append(hunk)
        elif hunk is not None and line.startswith("-"):
            hunk["removed"].append(line[1:])
        elif hunk is not None and line.startswith("+"):
            hunk["added"].append(line[1:])
    return files


def span(h):
    a, n = h["old_start"], h["old_count"]
    return (2 * a + 1, 2 * a + 1) if n == 0 else (2 * a, 2 * (a + n - 1))


def matches(path, pats):
    base = path.rsplit("/", 1)[-1]
    return any(fnmatch.fnmatch(path, p) or fnmatch.fnmatch(base, p) for p in pats)


def analyse(repo, main, refs, shared=None, resources=None):
    g = Git(repo)
    shared = shared or DEFAULT_SHARED
    resources = resources or dict(DEFAULT_RESOURCE)
    mainsha = g.out("rev-parse", "--verify", main + "^{commit}")
    L = {}
    for ref in refs:
        tip = g.out("rev-parse", "--verify", ref + "^{commit}")
        base = g.out("merge-base", mainsha, tip)
        revs, parents = [], {}
        for ln in g.out("rev-list", "--parents", f"{mainsha}..{tip}").splitlines():
            p = ln.split()
            revs.append(p[0]); parents[p[0]] = p[1:]
        L[ref] = {"lane": ref, "tip": tip, "merge_base": base, "own_commits": len(revs), "set": set(revs),
                  "parents": parents, "base": base, "contained_in": None, "carries": []}
    # containment
    contained = []
    order = list(refs)
    for a in order:
        for b in order:
            if a == b or L[a]["contained_in"] or L[b]["contained_in"]:
                continue
            ta, tb = L[a]["tip"], L[b]["tip"]
            same = ta == tb and order.index(a) > order.index(b)
            if same or (ta != tb and ta in L[b]["set"]):
                L[a]["contained_in"] = b
                contained.append({"lane": a, "in": b})
    live = [r for r in order if not L[r]["contained_in"]]
    # carried commits
    carried = {}
    for i, a in enumerate(live):
        for b in live[i + 1:]:
            common = L[a]["set"] & L[b]["set"]
            if not common:
                continue
            ps = set()
            for c in common:
                ps.update(L[a]["parents"].get(c, []))
            tips = [c for c in common if c not in ps]
            tip = sorted(tips)[0]
            e = carried.setdefault(tip, {"tip": tip, "commits": len(common), "lanes": set()})
            e["lanes"].update((a, b))
    for e in carried.values():
        for ln in e["lanes"]:
            cur = L[ln]["base"]
            if L[ln]["carries"] == [] or e["commits"] > max(c["commits"] for c in L[ln]["carries"]):
                L[ln]["base"] = e["tip"]
            L[ln]["carries"].append(e)
    carried_out = [{"tip": e["tip"], "commits": e["commits"], "lanes": sorted(e["lanes"], key=order.index)}
                   for e in carried.values()]
    # passes
    groups = {}
    for r in live:
        groups.setdefault(L[r]["base"], []).append(r)
    passes = []
    p1 = [{"base": b, "lanes": ls} for b, ls in groups.items() if b not in carried]
    for t, e in carried.items():
        p1.append({"base": g.out("merge-base", mainsha, t), "lanes": [], "carry_tip": t, "carried_by": sorted(e["lanes"], key=order.index)})
    p2 = [{"base": b, "lanes": ls, "carried": True} for b, ls in groups.items() if b in carried]
    if p1:
        passes.append(p1)
    if p2:
        passes.append(p2)
    # own diffs
    D = {}
    for r in live:
        base, tip = L[r]["base"], L[r]["tip"]
        hunks = parse_diff(g.run("diff", "-U0", "--no-color", "--no-renames", "--no-ext-diff", base, tip)[0])
        deleted, touched = set(), set(hunks)
        for ln in g.run("diff", "--name-status", "-M", "--no-ext-diff", base, tip)[0].splitlines():
            parts = ln.split("\t")
            if parts[0].startswith("D"):
                deleted.add(parts[1]); touched.add(parts[1])
            elif parts[0].startswith("R"):
                deleted.add(parts[1]); touched.update(parts[1:3])
            elif len(parts) > 1:
                touched.add(parts[-1])
        D[r] = {"hunks": hunks, "deleted": deleted, "touched": touched}
    # hotspots
    byfile = {}
    for r in live:
        for f in D[r]["touched"]:
            byfile.setdefault(f, []).append(r)
    hot, doubled = [], []
    for f in sorted(byfile):
        ls = byfile[f]
        for i, a in enumerate(ls):
            for b in ls[i + 1:]:
                kind, detail = None, None
                da, db = f in D[a]["deleted"], f in D[b]["deleted"]
                if matches(f, shared):
                    kind = "SHARED-HUB"
                elif da != db:
                    kind = "DELETE-EDIT"
                    detail = f"{a if da else b} deletes or renames it"
                else:
                    ov = [(x, y) for x in D[a]["hunks"].get(f, []) for y in D[b]["hunks"].get(f, [])
                          if max(span(x)[0], span(y)[0]) <= min(span(x)[1], span(y)[1])
                          and (x["added"], x["removed"]) != (y["added"], y["removed"])]
                    if ov:
                        kind = "SAME-LINES"
                        detail = "lines " + ", ".join(sorted({f"{x['old_start']}" + (f"-{x['old_start'] + x['old_count'] - 1}" if x["old_count"] > 1 else "") for x, _ in ov}, key=lambda s: int(s.split("-")[0])))
                        for x, y in ov:
                            if (x["old_count"] <= 1 and y["old_count"] <= 1 and len(x["added"]) <= 1 and len(y["added"]) <= 1
                                    and x["old_start"] == y["old_start"]):
                                doubled.append({"file": f, "line": x["old_start"], "lanes": [a, b],
                                                "action": "likely doubled line after stacking: keep one, check it appears once"})
                    elif not da:
                        kind = "CODE-BOTH"
                if kind:
                    hot.append({"kind": kind, "file": f, "lanes": [a, b], "detail": detail, "action": ACTION[kind]})
    hot.sort(key=lambda h: (RANK[h["kind"]], h["file"], h["lanes"]))
    # resource clashes and paired counts
    found = {}
    pats = {k: re.compile(v) for k, v in resources.items()}
    custom = set(resources) != set(DEFAULT_RESOURCE) or resources != DEFAULT_RESOURCE
    for r in live:
        for f, hs in D[r]["hunks"].items():
            for name, rx in pats.items():
                if not custom and "gate" not in f:
                    continue
                for h in hs:
                    for line in h["added"]:
                        for m in rx.finditer(line):
                            v = m.group(1) if rx.groups else m.group(0)
                            found.setdefault((name, v), {}).setdefault(r, set()).add(f)
    clashes = [{"name": n, "value": v, "lanes": [x for x in live if x in d], "files": sorted({f for s in d.values() for f in s}),
                "action": "give each lane its own value before the gates run in parallel"}
               for (n, v), d in sorted(found.items()) if len(d) > 1]
    paired = []
    for r in live:
        tip = L[r]["tip"]
        for f, hs in D[r]["hunks"].items():
            for h in hs:
                for old, new in zip(h["removed"], h["added"]):
                    if not PAIRED_LINE.search(new):
                        continue
                    on, nn = re.findall(r"\d+", old), re.findall(r"\d+", new)
                    if len(on) != len(nn) or on == nn:
                        continue
                    nm = re.search(r"const ([A-Z_]+) =", new) or re.search(r"\b([A-Z][A-Z0-9_]{2,})\b", new)
                    if not nm:
                        continue
                    name = nm.group(1)
                    o = next(x for x, y in zip(on, nn) if x != y)
                    n_ = next(y for x, y in zip(on, nn) if x != y)
                    out, rc = g.run("grep", "-n", "-I", "-F", name, tip, "--", ok=(0, 1))
                    seen = set()
                    for gl in out.splitlines():
                        parts = gl.split(":", 3)
                        if len(parts) < 4 or parts[1] == f or parts[1] in seen:
                            continue
                        if re.search(rf"(?<!\d){o}(?!\d)", parts[3]):
                            seen.add(parts[1])
                            paired.append({"lane": r, "name": name, "old": o, "new": n_, "file": f, "stale_in": parts[1],
                                           "line": int(parts[2]), "heuristic": True,
                                           "action": f"count changed here but its check still says {o}: update {parts[1]}:{parts[2]}"})
    # plan: greedy colouring of the conflict graph
    edges = {r: set() for r in live}
    for h in hot:
        if h["kind"] != "SHARED-HUB":
            a, b = h["lanes"]
            edges[a].add(b); edges[b].add(a)
    colour = {}
    for r in sorted(live, key=lambda x: (-len(edges[x]), live.index(x))):
        used = {colour[n] for n in edges[r] if n in colour}
        c = 0
        while c in used:
            c += 1
        colour[r] = c
    ngroups = (max(colour.values()) + 1) if colour else 0
    groups_out = [[r for r in live if colour[r] == c] for c in range(ngroups)]
    conflicts = sum(1 for h in hot if RANK[h["kind"]] == 0)
    hubs = sum(1 for h in hot if h["kind"] == "SHARED-HUB")
    summary = {"lanes": len(live), "independent_groups": ngroups, "certain_conflicts": conflicts,
               "shared_hubs": hubs, "resource_clashes": len(clashes)}
    line = f"{len(live)} lanes: {ngroups} independent groups; {conflicts} certain conflicts; {hubs} shared hubs; {len(clashes)} resource clashes"
    lanes_out = [{k: v for k, v in L[r].items() if k not in ("set", "parents", "carries")} for r in order]
    return {"main": main, "lanes": lanes_out, "contained": contained, "carried": carried_out,
            "passes": [[{k: v for k, v in x.items()} for x in p] for p in passes],
            "hotspots": hot, "doubled_line_risk": doubled, "resource_clashes": clashes, "paired_counts": paired,
            "groups": groups_out, "conflict_edges": {k: sorted(v) for k, v in edges.items() if v},
            "summary": summary, "summary_line": line}


def render(rep):
    s = lambda x: x[:9]
    o = [f"lanes against {rep['main']}", "", "1. bases and containment"]
    for l in rep["lanes"]:
        if l["contained_in"]:
            o.append(f"   {l['lane']}: contained in {l['contained_in']}: list only {l['contained_in']}")
        else:
            o.append(f"   {l['lane']}: merge-base {s(l['merge_base'])}, {l['own_commits']} own commits"
                     + (f", base for stacking {s(l['base'])}" if l["base"] != l["merge_base"] else "")
                     + ("  (no own commits: nothing to stack)" if l["own_commits"] == 0 else ""))
    for c in rep["carried"]:
        o.append(f"   both carry {c['commits']} commits up to {s(c['tip'])} ({' and '.join(c['lanes'])}): "
                 f"stack with that tip as lane base")
    for i, p in enumerate(rep["passes"], 1):
        for x in p:
            if x.get("carry_tip"):
                o.append(f"   pass {i}: stack the carried commits up to {s(x['carry_tip'])} (base {s(x['base'])}) on main, once, for {' and '.join(x['carried_by'])}")
                continue
            where = "carried tip, after pass 1" if x.get("carried") else "on main"
            o.append(f"   pass {i}: base {s(x['base'])} ({where}): {', '.join(x['lanes'])}")
    o += ["", "2. collision hotspots"]
    if not rep["hotspots"]:
        o.append("   none")
    for h in rep["hotspots"]:
        o.append(f"   {h['kind']:<11} {h['file']}  [{' x '.join(h['lanes'])}]" + (f" {h['detail']}" if h["detail"] else "")
                 + f" -> {h['action']}")
    o += ["", "3. doubled-line risk"]
    if not rep["doubled_line_risk"]:
        o.append("   none")
    for d in rep["doubled_line_risk"]:
        o.append(f"   {d['file']}:{d['line']} [{' x '.join(d['lanes'])}] -> {d['action']}")
    o += ["", "4. resource clashes"]
    if not rep["resource_clashes"]:
        o.append("   none")
    for c in rep["resource_clashes"]:
        o.append(f"   {c['name']}={c['value']} introduced by {' and '.join(c['lanes'])} ({', '.join(c['files'])}) -> {c['action']}")
    o += ["", "5. paired counts (heuristic)"]
    if not rep["paired_counts"]:
        o.append("   none")
    for p in rep["paired_counts"]:
        o.append(f"   {p['lane']}: {p['name']} {p['old']} -> {p['new']} in {p['file']}; count changed here but its check still says {p['old']} "
                 f"in {p['stale_in']}:{p['line']} -> update it")
    o += ["", "6. parallel plan"]
    for i, gp in enumerate(rep["groups"], 1):
        o.append(f"   group {i}: {', '.join(gp)}" + ("" if i == 1 else "  (after the groups above; conflicts with them)"))
    o.append(rep["summary_line"])
    return "\n".join(o)


def parse_resource(items):
    out = {}
    for it in items or []:
        if "=" not in it:
            raise SystemExit(f"--resource-regex wants NAME=REGEX, got {it!r}")
        n, rx = it.split("=", 1)
        try:
            re.compile(rx)
        except re.error as e:
            raise SystemExit(f"bad regex for {n}: {e}")
        out[n] = rx
    return out or None


def cmd_lanes(a):
    shared = [x.strip() for x in a.shared_files.split(",") if x.strip()] if a.shared_files else None
    try:
        rep = analyse(a.repo, a.main, a.lane_ref, shared, parse_resource(a.resource_regex))
    except GitError as e:
        print(f"git failed: {e}", file=sys.stderr)
        return 2
    print(json.dumps(rep, indent=2) if a.json else render(rep))
    return 0
