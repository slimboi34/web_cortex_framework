"""Build benchmarks/user_workflows/regression.ipynb from cells written here.

    python build_notebook.py regression.ipynb
"""
import sys

import nbformat as nbf

OUT = sys.argv[1] if len(sys.argv) > 1 else "regression.ipynb"
cells = []


def md(text):
    cells.append(nbf.v4.new_markdown_cell(text.strip()))


def code(text):
    cells.append(nbf.v4.new_code_cell(text.strip()))


md("""
# WebCortex: 2.5.3 vs 2.6.0, as a user would run it

This notebook runs WebCortex the way people use it. It builds apps and serves them. It
calls them as a browser, a `curl` user, an MCP client such as Claude Code, an agent with a
model behind it, and a camera on a WebSocket. It puts the **published 2.5.3 wheel** next to
the **development build** on every scenario.

* **Everyday scenarios:** a bookstore CRUD app, typed handlers, OpenAPI, an MCP session,
  agents with tools, handoffs, behaviours and flows, a camera stream, and the CLI on a
  fresh starter. Both builds should behave the same on these.
* **Edge cases:** each one is a bug fixed in 2.6.0, written the way someone would actually
  run into it. The expected value is what a correct framework does.
* **Isolation:** each scenario runs in its own interpreter against real servers on
  ephemeral ports, so a crash or hang in one cannot affect the next.
* **No real model API is called.** Agents run against WebCortex's offline fake provider,
  or a stub OpenAI-compatible server started inside the scenario.

**To run it:** use the kernel from `~/web_cortex_framework/.venv`, after
`maturin develop --release`. The first run installs the 2.5.3 wheel into
`~/.cache/webcortex-baseline/2.5.3`.
""")

code("""
import json, os, sys, time
sys.path.insert(0, os.path.abspath("."))
import pandas as pd
import matplotlib.pyplot as plt
from IPython.display import display, Markdown

import cases

pd.set_option("display.max_colwidth", 90)
pd.set_option("display.width", 220)
BUILDS = {"2.5.3": cases.baseline_path("2.5.3"), "2.6.0 (dev)": cases.dev_path()}
for label, path in BUILDS.items():
    print(f"{label:12} {path}")
everyday = [c for c in cases.CASES if c.everyday]
edge = [c for c in cases.CASES if not c.everyday]
print(f"{len(cases.CASES)} scenarios: {len(everyday)} everyday, {len(edge)} edge cases, "
      f"in {len({c.section for c in cases.CASES})} areas")
""")

md("""
## 1. Every scenario on both builds

`✅` means the app behaved correctly. `❌` lists the observations that were wrong,
with the correct value in brackets.
""")

code("""
started = time.perf_counter()
outcomes = {label: [cases.run_case(c, path) for c in cases.CASES] for label, path in BUILDS.items()}
verdicts = {label: [cases.judge(c, o) for c, o in zip(cases.CASES, outs)] for label, outs in outcomes.items()}
print(f"ran {sum(len(v) for v in outcomes.values())} scenario processes in {time.perf_counter() - started:.0f}s")


def cell(v):
    return "✅" if v["ok"] else "❌ " + (v["why"][:110] + ("…" if len(v["why"]) > 110 else ""))


rows = []
for i, c in enumerate(cases.CASES):
    row = {"area": c.section, "scenario": c.label, "kind": "everyday" if c.everyday else "edge"}
    for label in BUILDS:
        row[label] = cell(verdicts[label][i])
    rows.append(row)
table = pd.DataFrame(rows)
display(table.style.set_properties(**{"text-align": "left"}).hide(axis="index"))
""")

md("""
## 2. Scoreboard by area
""")

code("""
score = (table.assign(**{label: [v["ok"] for v in verdicts[label]] for label in BUILDS})
         .groupby("area")[list(BUILDS)].agg(["sum", "count"]))
summary = pd.DataFrame({label: score[label]["sum"].astype(str) + " / " + score[label]["count"].astype(str)
                        for label in BUILDS})
display(summary)

passed = pd.DataFrame({label: score[label]["sum"] for label in BUILDS})
ax = passed.plot.barh(figsize=(8, 4), color=["#c0504d", "#4f81bd"], width=0.75)
ax.set_xlabel("scenarios passed")
ax.set_title("Scenarios passed, by area")
ax.invert_yaxis()
plt.tight_layout()
plt.show()

for label in BUILDS:
    ok = sum(v["ok"] for v in verdicts[label])
    print(f"{label:12} passes {ok:2} / {len(cases.CASES)}")
""")

md("""
## 3. Everyday use: does anything regress?

These scenarios are ordinary use, and both builds should produce the same observations.
The table shows every observation, not only the ones the scenario checks, along with
any difference between the builds.
""")

code("""
diffs = []
for i, c in enumerate(cases.CASES):
    if not c.everyday:
        continue
    a, b = (outcomes[label][i].get("value") for label in BUILDS)
    if a is None or b is None:
        diffs.append({"scenario": c.label, "observation": "(run failed)", "2.5.3": a, "2.6.0": b})
        continue
    for key in sorted(set(a) | set(b)):
        if a.get(key) != b.get(key):
            diffs.append({"scenario": c.label, "observation": key, "2.5.3": a.get(key), "2.6.0": b.get(key)})
if diffs:
    display(pd.DataFrame(diffs))
else:
    display(Markdown(f"**No differences.** All {len(everyday)} everyday scenarios produced identical "
                     "observations on both builds."))
""")

md("""
## 4. What 2.5.3 did, case by case

For each edge case: what 2.5.3 actually observed, next to what 2.6.0 observed.
""")

code("""
for i, c in enumerate(cases.CASES):
    if c.everyday:
        continue
    old, new = (outcomes[label][i] for label in BUILDS)
    display(Markdown(f"**{c.section} — {c.label}.** {c.why}"))
    keys = list(c.expect)
    frame = pd.DataFrame({
        "observation": keys,
        "correct": [json.dumps(c.expect[k], default=str) for k in keys],
        "2.5.3": [json.dumps(old.get("value", {}).get(k, old.get("error")), default=str)[:80] for k in keys],
        "2.6.0": [json.dumps(new.get("value", {}).get(k, new.get("error")), default=str)[:80] for k in keys],
    })
    display(frame.style.hide(axis="index"))
""")

md("""
## 5. Throughput

The same app runs on each build: a Rust-executed static route, a Rust SQL route from
`app.resource`, and a Python handler. Eight keep-alive clients hit each one. The rounds
alternate between builds and the best of three is kept, because machine load moves these
numbers by about ±20%. The clients are Python, so they cap the rate at about the same
level on both builds. This is a regression check, not a peak benchmark; `ab` or `oha`
measure the peak.
""")

code("""
timing_case = cases.Case("timing", "throughput", cases.TIMING, {})
rates = {label: {} for label in BUILDS}
for _ in range(2):
    for label, path in BUILDS.items():
        out = cases.run_case(timing_case, path)
        for route, rps in out["value"].items():
            rates[label][route] = max(rates[label].get(route, 0), rps)
speed = pd.DataFrame(rates).round(0).astype(int)
speed["2.6.0 / 2.5.3"] = (speed["2.6.0 (dev)"] / speed["2.5.3"]).round(2)
speed.index = ["/ping (Rust static)", "/items (Rust SQL)", "/py (Python handler)"]
display(speed)
ax = speed[list(BUILDS)].plot.bar(figsize=(7, 3.5), color=["#c0504d", "#4f81bd"], rot=0)
ax.set_ylabel("requests / second")
ax.set_title("Keep-alive throughput, 8 clients")
plt.tight_layout()
plt.show()
""")

md("""
## 6. Verdict
""")

code("""
old_fails = sum(not v["ok"] for v in verdicts["2.5.3"])
new_fails = sum(not v["ok"] for v in verdicts["2.6.0 (dev)"])
regressions = [c.label for i, c in enumerate(cases.CASES)
               if verdicts["2.5.3"][i]["ok"] and not verdicts["2.6.0 (dev)"][i]["ok"]]
slowest = speed["2.6.0 / 2.5.3"].min()
print(f"scenarios: {len(cases.CASES)} | 2.5.3 fails {old_fails} | 2.6.0 fails {new_fails} | "
      f"regressions {len(regressions)} | everyday differences {len(diffs)} | "
      f"slowest route vs 2.5.3: {slowest:.2f}x")
assert new_fails == 0, [c.label for i, c in enumerate(cases.CASES) if not verdicts["2.6.0 (dev)"][i]["ok"]]
assert not regressions, regressions
assert not diffs, diffs
assert slowest > 0.8, speed
""")

nb = nbf.v4.new_notebook()
nb["cells"] = cells
nb["metadata"]["kernelspec"] = {"name": "python3", "display_name": "Python 3", "language": "python"}
nbf.write(nb, OUT)
print("wrote", OUT, len(cells), "cells")
