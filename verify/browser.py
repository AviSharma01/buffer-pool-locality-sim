"""Open the viewer in a real browser and check it.

    python verify/browser.py      # needs the release engine and Playwright

For a set of runs (every shipped trace, a free-page scenario, tiny and large
pools, read-ahead on and off), on a desktop page in light mode and a phone page
in dark mode:

- no script errors, no horizontal overflow, no "does not add up" warning;
- every access can be shown, on SSD and on HDD, and every pool opened;
- each pool's total time on SSD and on HDD equals the independent model's;
- race, the arrow keys and dragging on the chart work, and dropping a run file
  of an older version shows an error instead of a page.

Install Playwright once: pip install playwright && python -m playwright install chromium
"""

from __future__ import annotations

import json
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import compare  # noqa: E402
import fuzz  # noqa: E402
import model  # noqa: E402

try:
    from playwright.sync_api import Page, sync_playwright
except ImportError:
    print(
        __doc__.split("Install Playwright once: ")[1].strip().join(["Playwright is missing: ", ""])
    )
    sys.exit(2)

TRACE_DIR = compare.REPO / "traces"
POLICIES_WITH_A_MODEL = list(model.POLICIES)  # Optimal may break ties differently


def long_trace(work: Path) -> Path:
    """More page requests than the timeline has pixels, so it shades columns."""
    rng = random.Random(5)
    trace = fuzz.random_trace(rng, 0)
    trace["scenario"] = "long"
    pages = trace["page_count"]
    trace["queries"] = [
        {"title": f"batch {q + 1}", "sql": "SELECT 1", "plan": [], "scans": [],
         "io": [["r", rng.randint(1, pages)] if rng.random() < 0.4 else ["r", 1 + (i % pages)]
                for i in range(500)]}
        for q in range(5)
    ]  # fmt: skip
    path = work / "long.trace.json"
    path.write_text(json.dumps(trace), encoding="utf-8")
    return path


def runs(work: Path) -> list[tuple[str, Path, list[str]]]:
    """(label, trace, engine arguments) for every run to check."""
    traces = compare.traces(work)
    traces["long: 2,500 requests"] = long_trace(work)
    out = [(name, path, ["--policy", "all", "--frames", "8"]) for name, path in traces.items()]
    out += [
        ("growth-vacuum, 32 frames, no read-ahead, HDD", TRACE_DIR / "growth-vacuum.trace.json",
         ["--policy", "all", "--frames", "32", "--read-ahead", "off", "--storage", "hdd"]),
        ("shop, LRU only, 2 frames, HDD", TRACE_DIR / "shop.trace.json",
         ["--policy", "lru", "--frames", "2", "--storage", "hdd"]),
        ("shop, 1 frame", TRACE_DIR / "shop.trace.json", ["--policy", "all", "--frames", "1"]),
    ]  # fmt: skip
    return out


def check_page(page: Page, label: str, trace: Path, run: dict, width: int) -> list[str]:
    problems = []
    accesses = page.evaluate("bpsim.accesses()")
    for storage in model.PROFILES:
        page.click("#sSsd" if storage == "ssd" else "#sHdd")
        page.evaluate("n => { for (let i = -1; i < n; i++) bpsim.jump(i); }", accesses)
        if width < 1000:
            continue
        read_ahead = "off" if run["runs"][0]["summary"]["read_ahead_cap"] == 0 else "auto"
        for r in run["runs"]:
            if r["policy"] not in POLICIES_WITH_A_MODEL:
                continue
            shown = page.evaluate("p => bpsim.running(p).c[bpsim.accesses() - 1]", r["policy"])
            want = model.run(trace, r["policy"], run["frames"], storage, read_ahead)
            if abs(shown - want["total_ms"]) > 1e-9 * max(1.0, want["total_ms"]):
                problems.append(
                    f"{r['policy']} on {storage}: page shows {shown} ms, "
                    f"model {want['total_ms']} ms"
                )
            # the number a reader sees in the score column, not just the page's state
            seen = page.inner_text(f'#slist [data-p="{r["policy"]}"] .h')
            if seen != str(want["hits"]):
                problems.append(
                    f"{r['policy']}: score column shows {seen} hits, model {want['hits']}"
                )
    # every pool opened in turn, then all pools shown and hidden again
    for row in page.query_selector_all("#slist .srow"):
        row.click()
        if page.query_selector_all("#pools .pool.focus") == []:
            problems.append(f"clicking {row.inner_text().splitlines()[0]} opened no pool")
    page.click("#allBtn")
    if len(page.query_selector_all("#pools .pool")) != len(run["runs"]):
        problems.append("Show all pools did not draw every pool")
    page.click("#allBtn")
    if len(page.query_selector_all("#pools .pool")) != 1:
        problems.append("hiding all pools left more than the open one")
    at = page.evaluate("bpsim.at()")
    if at >= 0:
        if len(page.query_selector_all("#cells i.now")) != 1:
            problems.append("the disk strip does not mark exactly one current page")
        if len(page.query_selector_all("#cells i.head")) > 1:
            problems.append("the disk strip marks more than one read head")
        if not page.inner_text("#pos").startswith(f"{at + 1} of "):
            problems.append(f"the timeline says {page.inner_text('#pos')!r} at access {at + 1}")
    painted = page.evaluate(
        """() => { const c = document.getElementById('tlCanvas');
        const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
        let n = 0; for (let i = 3; i < d.length; i += 4) if (d[i]) n++;
        return n / (d.length / 4); }"""
    )
    if painted < 0.5:
        problems.append(f"the timeline strip is mostly blank ({painted:.0%} painted)")
    page.evaluate("() => { document.getElementById('fold').open = true; }")
    overflow = page.evaluate("() => document.documentElement.scrollWidth")
    if overflow > width:
        problems.append(f"page is {overflow}px wide in a {width}px window")
    if page.is_visible("#warn"):
        problems.append("warning shown: " + page.inner_text("#warn"))
    return problems


def check_interaction(page: Page, run_file: Path) -> list[str]:
    problems = []
    page.evaluate("() => bpsim.jump(-1)")
    page.keyboard.press("ArrowRight")
    page.keyboard.press("ArrowRight")
    if page.evaluate("bpsim.at()") != 1:
        problems.append(
            f"two right-arrow presses landed on access {page.evaluate('bpsim.at()')}, not 1"
        )
    box = page.query_selector("#tlCanvas").bounding_box()  # the timeline strip
    page.mouse.move(box["x"] + box["width"] * 0.6, box["y"] + box["height"] / 2)
    page.mouse.down()
    page.mouse.move(box["x"] + box["width"] * 0.7, box["y"] + box["height"] / 2)
    page.mouse.up()
    want = int(0.7 * page.evaluate("bpsim.accesses()"))
    if abs(page.evaluate("bpsim.at()") - want) > 1:
        landed = page.evaluate("bpsim.at()")
        problems.append(f"dragging on the timeline landed on {landed}, not ~{want}")
    page.click("#allBtn")
    page.click("#mRace")
    page.select_option("#speed", index=0)
    page.click("#play")
    page.wait_for_function("() => !bpsim.playing()", timeout=60_000)
    if page.query_selector_all("#slist .place.on") == []:
        problems.append("the race finished without placing any pool")
    page.click("#mStep")
    old = json.loads(run_file.read_text(encoding="utf-8"))
    old["version"] -= 1
    drop = """t => {
        const dt = new DataTransfer();
        dt.items.add(new File([t], 'run.json'));
        const init = {dataTransfer: dt, bubbles: true, cancelable: true};
        document.dispatchEvent(new DragEvent('drop', init));
    }"""
    page.evaluate(drop, json.dumps(old))
    page.wait_for_timeout(200)
    if "expected bpsim-run" not in page.inner_text("#error"):
        problems.append("an older run file was not refused with a clear error")
    return problems


def main() -> int:
    if not compare.SIM.is_file():
        print(f"build the engine first: cd sim && cargo build --release ({compare.SIM} missing)")
        return 2
    problems: list[str] = []
    checked = 0
    with tempfile.TemporaryDirectory() as tmp, sync_playwright() as pw:
        work = Path(tmp)
        browser = pw.chromium.launch()
        for i, (label, trace, args) in enumerate(runs(work)):
            out = work / f"run{i}"
            shutil.rmtree(out, ignore_errors=True)
            cmd = [str(compare.SIM), "--trace", str(trace), *args, "--out", str(out)]
            subprocess.run(cmd, check=True, capture_output=True)
            viewer = next(out.glob("**/viewer.html"))
            run = json.loads((viewer.parent / "run.json").read_text(encoding="utf-8"))
            for width, scheme in [(1280, "light"), (400, "dark")]:
                page = browser.new_page(
                    viewport={"width": width, "height": 1000}, color_scheme=scheme
                )
                errors: list[str] = []
                page.on("pageerror", lambda e, errors=errors: errors.append(str(e)))
                page.goto(viewer.as_uri())
                page.wait_for_selector(".pool")
                found = check_page(page, label, trace, run, width)
                if width == 1280 and i == 0:
                    found += check_interaction(page, viewer.parent / "run.json")
                found += [f"script error: {e}" for e in errors]
                problems += [f"{label} ({width}px {scheme}): {p}" for p in found]
                checked += 1
                page.close()
        browser.close()
    for p in problems:
        print("PROBLEM", p)
    print(f"{checked} pages checked, {len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
