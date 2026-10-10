#!/usr/bin/env python3
"""Stress lmux and cmux with the same workload and measure what each costs.

    deploy/bench/stress.py lmux 5 10 20 40     # steps: workspaces open at once
    deploy/bench/stress.py cmux 5 10 20
    deploy/bench/stress.py herdr 5 10 20 40    # needs `herdr --session bench` running in a tmux

Each workspace gets a terminal printing ~20 lines/s and an idle split. At
every step the script samples the app's own processes (shells and agents are
the workload, not the app, so they are left out), times an agent's send ->
read-screen round trip, and times a 200k-line burst into a visible pane. The
baseline before anything is added is subtracted, so sessions already open in
the app cancel out. Everything it opens is closed at the end.
"""
import json, os, re, statistics, subprocess, sys, time

APP = sys.argv[1]
STEPS = [int(n) for n in sys.argv[2:]] or [5, 10, 20]
CWD = "/tmp/lmux-bench"
os.makedirs(CWD, exist_ok=True)
CHATTY = "i=0; while :; do i=$((i+1)); echo \"tick $i $(date +%S)\"; sleep 0.05; done"
LMUX = "/Applications/lmux.app/Contents/MacOS/lmux"
CMUX = "/Applications/cmux.app/Contents/Resources/bin/cmux"
HERDR = ["herdr", "--session", "bench"]
ENV = dict(os.environ, CMUX_QUIET="1")
for k in ("LMUX_WORKSPACE_ID", "LMUX_SURFACE_ID"):
    ENV.pop(k, None)


def sh(*argv, check=True):
    r = subprocess.run(argv, capture_output=True, text=True, env=ENV)
    if check and r.returncode != 0:
        raise RuntimeError(f"{argv}: {r.stderr.strip()}")
    return r.stdout


# --- the two apps ------------------------------------------------------------

class Lmux:
    def procs(self, rows):
        return [r for r in rows if re.search(r"lmux\.app/Contents/MacOS/|^tmux -L lmux |/tmux -L lmux ", r[3])]

    def new(self, i):
        r = json.loads(sh(LMUX, "--json", "new-workspace", "--name", f"bench-{i}", "--cwd", CWD, "--command", f"sh -c '{CHATTY}'"))
        sh(LMUX, "new-pane", "--surface", r["surface"], "--direction", "right", "--focus", "false", "--cwd", CWD)
        return r["workspace"], r["surface"]

    def idle_tab(self, ws):
        tree = json.loads(sh(LMUX, "--json", "tree"))
        w = next(w for w in tree["workspaces"] if w["id"] == ws)
        return w["panes"][1]["surfaces"][0]["id"]

    def send(self, ws, s, text):
        sh(LMUX, "send", "--surface", s, text)

    def screen(self, ws, s):
        return sh(LMUX, "read-screen", "--surface", s)

    def select(self, ws):
        sh(LMUX, "select-workspace", ws)

    def selected(self):
        return sh(LMUX, "current-workspace").strip()

    def close(self, ws):
        sh(LMUX, "close-workspace", ws, check=False)


class Cmux:
    def procs(self, rows):
        return [r for r in rows if re.search(r"/Applications/cmux\.app/|\.cmux/ssh|cmux-cua serve", r[3])
                and "claude" not in r[3].split()[0]]

    def new(self, i):
        out = sh(CMUX, "new-workspace", "--name", f"bench-{i}", "--cwd", CWD, "--focus", "false", "--command", f"sh -c '{CHATTY}'")
        ws = re.search(r"workspace:\d+", out).group(0)
        first = re.search(r"surface:\d+", sh(CMUX, "tree", "--workspace", ws)).group(0)
        sh(CMUX, "new-split", "right", "--workspace", ws, "--surface", first)
        return ws, first

    def idle_tab(self, ws):
        return re.findall(r"surface:\d+", sh(CMUX, "tree", "--workspace", ws))[1]

    def send(self, ws, s, text):
        sh(CMUX, "send", "--workspace", ws, "--surface", s, text)

    def screen(self, ws, s):
        return sh(CMUX, "read-screen", "--workspace", ws, "--surface", s)

    def select(self, ws):
        sh(CMUX, "select-workspace", "--workspace", ws)

    def selected(self):
        for line in sh(CMUX, "list-workspaces").splitlines():
            if "[selected]" in line:
                return re.search(r"workspace:\d+", line).group(0)

    def close(self, ws):
        sh(CMUX, "close-workspace", "--workspace", ws, "--force", check=False)


class Herdr:
    """herdr's server plus its TUI client (run in a tmux, so it renders)."""
    def __init__(self):
        self.split = {}

    def procs(self, rows):
        return [r for r in rows if re.search(r"(^|/)herdr( --session bench| server)$", r[3])]

    def j(self, *argv):
        return json.loads(sh(*HERDR, *argv))["result"]

    def new(self, i):
        r = self.j("workspace", "create", "--cwd", CWD, "--label", f"bench-{i}", "--no-focus")
        ws, root = r["workspace"]["workspace_id"], r["root_pane"]["pane_id"]
        sh(*HERDR, "pane", "run", root, f"sh -c '{CHATTY}'")
        idle = self.j("pane", "split", root, "--direction", "right", "--cwd", CWD, "--no-focus")["pane"]["pane_id"]
        self.split[ws] = idle
        return ws, root

    def idle_tab(self, ws):
        return self.split[ws]

    def send(self, ws, s, text):
        sh(*HERDR, "pane", "run", s, text.rstrip("\n"))

    def screen(self, ws, s):
        return sh(*HERDR, "pane", "read", s, "--source", "visible")

    def select(self, ws):
        sh(*HERDR, "workspace", "focus", ws, check=False)

    def selected(self):
        for w in self.j("workspace", "list")["workspaces"]:
            if w.get("focused"):
                return w["workspace_id"]

    def close(self, ws):
        sh(*HERDR, "workspace", "close", ws, check=False)


app = {"lmux": Lmux, "cmux": Cmux, "herdr": Herdr}[APP]()


# --- measurements ------------------------------------------------------------

def ps_rows():
    rows = []
    for line in sh("ps", "-axo", "pid=,rss=,%cpu=,command=").splitlines():
        p = line.split(None, 3)
        if len(p) == 4:
            rows.append((int(p[0]), int(p[1]), float(p[2]), p[3]))
    return rows


def footprint_mb(pids):
    """Physical footprint (what Activity Monitor shows), summed."""
    total = 0.0
    out = sh("footprint", *map(str, pids), check=False)
    for m in re.finditer(r"Footprint: ([\d.]+) (KB|MB|GB)", out):
        v = float(m.group(1))
        total += v / 1024 if m.group(2) == "KB" else v * 1024 if m.group(2) == "GB" else v
    return total


def sample(seconds=12):
    """App CPU (sum of its processes, % of one core) averaged with top, and memory."""
    pids = [r[0] for r in app.procs(ps_rows())]
    args = ["top", "-l", str(seconds // 2 + 1), "-s", "2", "-stats", "pid,cpu"]
    for p in pids:
        args += ["-pid", str(p)]
    out = sh(*args, check=False)
    names = {r[0]: proc_name(r[3]) for r in app.procs(ps_rows())}
    by_name = {}
    per_sample, cur = [], None
    for line in out.splitlines():
        if line.startswith("Processes:"):
            if cur is not None:
                per_sample.append(cur)
            cur = 0.0
        m = re.match(r"\s*(\d+)\s+([\d.]+)", line)
        if m and cur is not None and int(m.group(1)) in pids:
            cur += float(m.group(2))
            if per_sample:
                nm = names.get(int(m.group(1)), "?")
                by_name[nm] = by_name.get(nm, 0.0) + float(m.group(2))
    if cur is not None:
        per_sample.append(cur)
    cpu = statistics.mean(per_sample[1:]) if len(per_sample) > 1 else 0.0  # the first top sample is since boot
    k = max(len(per_sample) - 1, 1)
    split = {nm: round(v / k, 1) for nm, v in sorted(by_name.items(), key=lambda x: -x[1]) if v / k >= 0.2}
    return {"procs": len(pids), "cpu": cpu, "mem_mb": footprint_mb(pids), "cpu_by": split}


def proc_name(cmd):
    exe = cmd.split()[0].rsplit("/", 1)[-1]
    if exe == "lmux" and " server" in cmd:
        return "daemon"
    if exe == "herdr":
        return "server" if cmd.rstrip().endswith("server") else "client"
    return {"lmux-app": "app", "cmux": "cmux"}.get(exe, exe)


def roundtrip(ws, s, n=15):
    """An agent's view: type a marker, poll the screen until it shows."""
    times = []
    for k in range(n):
        n = int(time.time() * 1000) % 100000
        mark = f"RT{k}x{n + 1}"
        t = time.perf_counter()
        app.send(ws, s, f"echo RT{k}x$(({n}+1))\n")
        while mark not in app.screen(ws, s) and time.perf_counter() - t < 5:
            pass
        times.append((time.perf_counter() - t) * 1000)
    return statistics.median(times)


def burst(ws, s, lines=200_000):
    """Seconds for a visible pane to take a burst of output and be back."""
    n = int(time.time()) % 10000
    mark = f"DONE{n + 1}"
    app.send(ws, s, "clear\n")
    time.sleep(0.3)
    t = time.perf_counter()
    app.send(ws, s, f"yes 'lorem ipsum dolor sit amet consectetur' | head -n {lines}; echo DONE$(({n}+1))\n")
    while mark not in app.screen(ws, s):
        if time.perf_counter() - t > 120:
            return None
        time.sleep(0.02)
    return time.perf_counter() - t


# --- run ---------------------------------------------------------------------

was_selected = app.selected()
opened = []
results = {"app": APP, "steps": []}
try:
    base = sample()
    results["baseline"] = base
    print(f"{APP} baseline: {base['procs']} procs, {base['mem_mb']:.0f} MB, {base['cpu']:.1f}% CPU", flush=True)
    for n in STEPS:
        t = time.perf_counter()
        while len(opened) < n:
            opened.append(app.new(len(opened) + 1))
        create_s = time.perf_counter() - t
        time.sleep(6)
        m = sample()
        ws, _ = opened[-1]
        idle = app.idle_tab(ws)
        rt = roundtrip(ws, idle)
        app.select(ws)
        time.sleep(1.5)
        b = burst(ws, idle)
        app.select(was_selected)
        step = {"workspaces": n, "terminals": 2 * n, "create_s": round(create_s, 2), **m,
                "mem_delta_mb": m["mem_mb"] - base["mem_mb"], "cpu_delta": m["cpu"] - base["cpu"],
                "roundtrip_ms": round(rt, 1), "burst_s": None if b is None else round(b, 2)}
        results["steps"].append(step)
        print(f"{APP} {n:>3} ws / {2*n:>3} terms: +{step['mem_delta_mb']:.0f} MB, "
              f"{m['cpu']:.1f}% CPU (+{step['cpu_delta']:.1f}), round trip {rt:.0f} ms, "
              f"200k-line burst {step['burst_s']} s, created in {create_s:.1f} s  {m['cpu_by']}", flush=True)
finally:
    for ws, _ in opened:
        app.close(ws)
    if was_selected:
        try:
            app.select(was_selected)
        except Exception:
            pass
    out = f"/tmp/lmux-bench/{APP}-{int(time.time())}.json"
    json.dump(results, open(out, "w"), indent=1)
    print("saved", out)
