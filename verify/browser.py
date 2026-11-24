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

And the sweep page, for a sweep with SQLite's cache measured, a small one and
one of a workload: the lines are a faithful plot of the sweep's numbers, the
table shows them, the readout's "LRU would need N× the memory" holds when the
engine runs LRU at that size, and the switches, keys and theme work.

Install Playwright once: pip install playwright && python -m playwright install chromium
"""

from __future__ import annotations

import json
import math
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
    from playwright.sync_api import TimeoutError as PlaywrightTimeout
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


def flies_during(page: Page, seconds: float) -> int:
    """The most pages seen flying at once, sampled while something plays."""
    most = 0
    for _ in range(int(seconds * 10)):
        most = max(most, len(page.query_selector_all(".fly")))
        page.wait_for_timeout(100)
    return most


def check_how(page: Page) -> list[str]:
    """The "How it works" dialog: hint, every walkthrough step, Esc, focus."""
    problems = []
    if not page.is_visible("#howHint"):
        problems.append("the New here? hint is not shown on a first visit")
    at = page.evaluate("bpsim.at()")
    page.click("#howBtn")
    if not page.evaluate("document.getElementById('how').open"):
        return problems + ["How it works did not open"]
    steps = len(page.query_selector_all("#walkDots i"))
    if steps < 10:
        problems.append(f"the walkthrough has {steps} steps, expected the engine's 14")
    for k in range(steps):
        said = page.inner_text("#walkSaid").strip()
        if not said or not page.query_selector_all("#walkFig .tile"):
            problems.append(f"walkthrough step {k + 1} drew no pool or sentence")
        if not page.inner_text("#walkTally").startswith(f"{k + 1} of {steps}"):
            problems.append(f"walkthrough step {k + 1} says {page.inner_text('#walkTally')!r}")
        page.keyboard.press("ArrowRight")
    if not page.inner_text("#walkTally").startswith("1 of"):
        problems.append("Next on the last step did not start over")
    if page.evaluate("bpsim.at()") != at:
        problems.append("arrow keys in the dialog moved the run behind it")
    page.keyboard.press("Escape")
    if page.evaluate("document.getElementById('how').open"):
        problems.append("Esc did not close How it works")
    if page.evaluate("document.activeElement.id") != "howBtn":
        problems.append("closing How it works did not return focus to its button")
    page.reload()
    page.wait_for_selector(".pool")
    if page.is_visible("#howHint"):
        problems.append("the New here? hint came back after How it works was opened")
    return problems


def check_drawn(page: Page) -> list[str]:
    """Drawing pools beside the open one, and racing just those."""
    problems = []
    other = page.query_selector("#slist .srow:not(.on)")
    p = other.get_attribute("data-p")
    toggle = f'#slist [data-p="{p}"] .sdraw'
    page.click(toggle)
    if len(page.query_selector_all("#pools .pool")) != 2 or not page.is_visible("#mRace"):
        problems.append(f"drawing {p} beside the open pool did not give two pools and Race")
    page.click("#mRace")
    page.select_option("#speed", index=2)
    page.click("#play")
    page.wait_for_function("() => !bpsim.playing()", timeout=60_000)
    if len(page.query_selector_all("#slist .place.on")) != 2:
        problems.append("a race of two drawn pools did not place exactly two")
    page.click("#mStep")
    page.click(toggle)
    if len(page.query_selector_all("#pools .pool")) != 1 or page.is_visible("#mRace"):
        problems.append(f"undrawing {p} did not leave the open pool alone")
    return problems


def residency(events: list[dict], frames: int) -> list[tuple[int, int, int, int]]:
    """(frame, page, first, last) for each stay of a page in a frame, from the
    run file: what the memory chart should draw."""
    bars, open_ = [], [None] * frames
    for i, ev in enumerate(events):
        for f in range(frames):
            page = ev["frames"][f]["page"]
            if open_[f] and open_[f][1] != page:
                bars.append(tuple(open_[f]))
                open_[f] = None
            if page is not None and not open_[f]:
                open_[f] = [f, page, i, i]
            if open_[f]:
                open_[f][3] = i
    return bars + [tuple(o) for o in open_ if o]


def check_mem(page: Page, run: dict) -> list[str]:
    """Memory over time: closed at first; bars match the run file; the open
    pool's band comes first; pointing names the page; dragging moves."""
    problems = []
    if page.evaluate("document.getElementById('mem').open"):
        problems.append("Memory over time is open before being asked for")
    page.click("#mem summary")
    # The chart draws on the panel's toggle event, a moment after the click
    # (a canvas has a height before it is drawn), so wait for its labels.
    try:
        page.wait_for_function("() => document.querySelector('.memlabel')", timeout=3000)
    except PlaywrightTimeout:
        problems.append("memory chart drew nothing within 3 s of opening")
    focus = page.evaluate("bpsim.focus()")
    by = {r["policy"]: r for r in run["runs"]}
    for p, r in by.items():
        want = len(residency(r["events"], run["frames"]))
        got = page.evaluate("p => bpsim.bars(p)", p)
        if got != want:
            problems.append(f"memory chart has {got} bars for {p}, the run file gives {want}")
    labels = page.eval_on_selector_all(".memlabel", "ls => ls.map(l => l.textContent)")
    if len(labels) != 1:
        problems.append(f"memory chart labels {labels} with only the open pool drawn")
    # the longest stay in the open pool, pointed at in its middle
    frames, n = run["frames"], len(run["runs"][0]["events"])
    f, pg, s, e = max(residency(by[focus]["events"], frames), key=lambda b: b[3] - b[2])
    page.locator("#memCanvas").scroll_into_view_if_needed()
    box = page.query_selector("#memCanvas").bounding_box()
    row_h = max(30, min(120, frames * 12)) / frames
    page.mouse.move(
        box["x"] + box["width"] * (s + e + 1) / 2 / n, box["y"] + 18 + (f + 0.5) * row_h
    )
    if f"p{pg} " not in page.inner_text("#memSay"):
        problems.append(f"pointing at p{pg}'s bar reads {page.inner_text('#memSay')!r}")
    page.mouse.move(box["x"] + box["width"] * 0.3, box["y"] + 30)
    page.mouse.down()
    page.mouse.move(box["x"] + box["width"] * 0.4, box["y"] + 30)
    page.mouse.up()
    if abs(page.evaluate("bpsim.at()") - int(0.4 * n)) > 1:
        problems.append(f"dragging on the memory chart landed on {page.evaluate('bpsim.at()')}")
    page.click("#mem summary")
    return problems


def check_sound(page: Page) -> list[str]:
    """Off and silent by default; on gives the pop as a test; a change of
    pools pops once, however many changes come within 2 seconds; stepping
    through moments is silent; a race's end plays the win; the choice is
    remembered. (Headless browsers can't hear: the page records what it asked
    for.)"""
    problems = []
    rows = page.eval_on_selector_all("#slist .srow:not(.on)", "rs => rs.map(r => r.dataset.p)")
    a, b = rows[0], rows[1]
    page.click(f'#slist [data-p="{a}"] .sdraw')
    page.click(f'#slist [data-p="{a}"] .sdraw')
    if page.evaluate("bpsim.heard()"):
        problems.append("a sound played with sound off")
    page.click("#soundBtn")
    if page.evaluate("bpsim.heard()") != ["popIn"]:
        problems.append(f"switching sound on gave {page.evaluate('bpsim.heard()')}, not the pop")
    page.wait_for_timeout(2100)
    for p in (a, b, a):  # three changes in quick succession
        page.click(f'#slist [data-p="{p}"] .sdraw')
    if page.evaluate("bpsim.heard()") != ["popIn", "popIn"]:
        problems.append(
            f"three quick changes gave {page.evaluate('bpsim.heard()')[1:]}, not one pop"
        )
    page.wait_for_timeout(2100)
    page.click(f'#slist [data-p="{b}"] .sdraw')  # removing b leaves the open pool alone
    if page.evaluate("bpsim.heard()")[-1:] != ["popOut"]:
        problems.append(
            f"removing a pool gave {page.evaluate('bpsim.heard()')[-1:]}, not the falling pop"
        )
    page.wait_for_timeout(2100)
    count = len(page.evaluate("bpsim.heard()"))
    page.focus("#play")
    page.evaluate("() => bpsim.jump(-1)")
    for _ in range(page.evaluate("bpsim.accesses()")):
        page.keyboard.press("ArrowRight")
    if len(page.evaluate("bpsim.heard()")) != count:
        problems.append("stepping through the run made a sound")
    page.click(f'#slist [data-p="{a}"] .sdraw')
    page.click("#mRace")
    page.select_option("#speed", index=2)
    page.click("#play")
    page.wait_for_function("() => !bpsim.playing()", timeout=60_000)
    if page.evaluate("bpsim.heard()")[-1:] != ["win"]:
        problems.append(
            f"a finished race gave {page.evaluate('bpsim.heard()')[count:]}, not the win"
        )
    page.click("#mStep")
    page.click(f'#slist [data-p="{a}"] .sdraw')
    page.reload()
    page.wait_for_selector(".pool")
    if page.get_attribute("#soundBtn", "aria-pressed") != "true":
        problems.append("sound on was not remembered")
    page.click("#soundBtn")
    return problems


def check_changes(page: Page) -> list[str]:
    """Moments follow the pools on screen; Play keeps going through a change;
    a drawn pool's name swaps it to large; what just arrived glows, then not."""
    problems = []

    def strays() -> list[str]:
        shown = set(page.evaluate("bpsim.pools()"))
        off = [
            r.inner_text().splitlines()[0]
            for r in page.query_selector_all("#slist .srow")
            if r.get_attribute("data-p") not in shown and r.get_attribute("data-p") != "opt"
        ]
        text = " ".join(m["d"] for m in page.evaluate("bpsim.moments()"))
        return [n for n in off if n in text]  # fmt: skip

    if strays():
        problems.append(f"with one pool open, moments name {strays()}")
    other = page.eval_on_selector_all(
        "#slist .srow:not(.on)", "rs => rs.map(r => r.dataset.p).filter(p => p !== 'opt')"
    )[0]
    before = page.evaluate("bpsim.moments()")
    page.evaluate("() => bpsim.jump(-1)")
    page.select_option("#speed", index=2)
    page.click("#play")
    page.wait_for_timeout(600)
    page.click(f'#slist [data-p="{other}"] .sdraw')
    at = page.evaluate("bpsim.at()")
    if not page.evaluate("bpsim.playing()"):
        problems.append("drawing a pool stopped Play")
    page.wait_for_timeout(900)
    if page.evaluate("bpsim.at()") <= at:
        problems.append("Play did not move on after a pool was drawn")
    page.click("#play")
    if page.evaluate("bpsim.moments()") == before:
        problems.append(f"drawing {other} did not change the moments")
    if strays():
        problems.append(f"with {other} drawn, moments name {strays()}")
    if "fresh" not in (page.get_attribute(f'#slist [data-p="{other}"]', "class") or ""):
        problems.append(f"{other} did not glow on arriving")
    page.wait_for_timeout(2200)
    if page.query_selector_all(".fresh"):
        problems.append("the glow outlasted 2 seconds")
    opened = page.evaluate("bpsim.focus()")
    page.click(f'#slist [data-p="{other}"] .sopen')
    if page.evaluate("bpsim.focus()") != other or sorted(page.evaluate("bpsim.pools()")) != sorted(
        [opened, other]
    ):
        problems.append("opening a drawn pool did not swap it with the open one")
    page.click(f'#slist [data-p="{opened}"] .sopen')
    page.click(f'#slist [data-p="{other}"] .sdraw')
    return problems


def check_race_changes(page: Page) -> list[str]:
    """A race keeps running when the open pool is swapped, or a pool is drawn
    or removed; one drawn mid-race joins near the others, not at the start;
    places go to the pools still in it; under two pools it stops."""
    problems = []
    rows = page.eval_on_selector_all("#slist .srow:not(.on)", "rs => rs.map(r => r.dataset.p)")
    a, b, c = rows[0], rows[1], rows[2]
    page.click(f'#slist [data-p="{a}"] .sdraw')
    page.click(f'#slist [data-p="{b}"] .sdraw')
    page.click("#mRace")
    page.select_option("#speed", index=1)
    page.click("#play")
    page.wait_for_timeout(1500)
    page.click(f'#slist [data-p="{c}"] .sdraw')
    others = [
        page.evaluate("p => bpsim.racePos(p)", p) for p in page.evaluate("bpsim.pools()") if p != c
    ]
    joined = page.evaluate("p => bpsim.racePos(p)", c)
    if not page.evaluate("bpsim.playing()"):
        problems.append("drawing a pool stopped the race")
    elif joined is None or joined < min(others) / 2:
        problems.append(f"{c} joined the race at {joined}, the others were at {others}")
    page.click(f'#slist [data-p="{b}"] .sdraw')
    if not page.evaluate("bpsim.playing()"):
        problems.append("removing a pool stopped the race")
    page.click(f'#slist [data-p="{a}"] .sopen')
    if not page.evaluate("bpsim.playing()"):
        problems.append("opening a drawn pool stopped the race")
    page.select_option("#speed", index=2)
    page.wait_for_function("() => !bpsim.playing()", timeout=120_000)
    racing = sorted(page.evaluate("bpsim.pools()"))
    placed = sorted(
        page.eval_on_selector_all(
            "#slist .place.on", "ps => ps.map(p => p.closest('.srow').dataset.p)"
        )
    )
    if placed != racing:
        problems.append(f"places went to {placed}, the pools in the race were {racing}")
    gone = next(p for p in racing if p != page.evaluate("bpsim.focus()"))
    page.click(f'#slist [data-p="{gone}"] .sdraw')  # a finished pool leaves
    placed = sorted(
        page.eval_on_selector_all(
            "#slist .place.on", "ps => ps.map(p => p.closest('.srow').dataset.p)"
        )
    )
    if placed != sorted(p for p in racing if p != gone):
        problems.append(f"after {gone} left, places show {placed}")
    for p in page.evaluate("bpsim.pools()"):
        if p != page.evaluate("bpsim.focus()"):
            page.click(f'#slist [data-p="{p}"] .sdraw')
    if page.get_attribute("#mStep", "aria-pressed") != "true":
        problems.append("down to one pool, the race did not end")
    return problems


def check_highlights(page: Page) -> list[str]:
    """Drawing a pool says how many highlights came, and the new diamonds
    glow; both clear after 2 seconds; removing it says how many went."""
    problems = []
    if page.evaluate("bpsim.pools().length") != 1:
        return ["highlight check needs the open pool alone"]
    candidates = page.eval_on_selector_all(
        "#slist .srow:not(.on)", "rs => rs.map(r => r.dataset.p).filter(p => p !== 'opt')"
    )
    for p in candidates:
        before = {m["k"] for m in page.evaluate("bpsim.moments()")}
        page.click(f'#slist [data-p="{p}"] .sdraw')
        after = {m["k"] for m in page.evaluate("bpsim.moments()")}
        added, dropped = after - before, before - after
        note = page.inner_text("#tlNote")
        name = page.inner_text(f'#slist [data-p="{p}"] .n')
        if added and not note.startswith(f"+{len(added)} highlight"):
            problems.append(f"drawing {p} added {len(added)} highlights; the note says {note!r}")
        if added and f"with {name}" not in note:
            problems.append(f"the note {note!r} does not name {name}")
        if dropped and f"−{len(dropped)} highlight" not in note:
            problems.append(
                f"drawing {p} dropped {len(dropped)} highlights; the note says {note!r}"
            )
        glowing = {
            int(k)
            for k in page.eval_on_selector_all(
                "#tlMarks .mk.fresh", "ms => ms.map(m => m.dataset.k)"
            )
        }
        if glowing != added:
            problems.append(
                f"drawing {p}: diamonds glowing {sorted(glowing)}, new ones {sorted(added)}"
            )
        page.wait_for_timeout(2200)
        if page.inner_text("#tlNote") or page.query_selector_all(".mk.fresh"):
            problems.append("the highlight note or glow outlasted 2 seconds")
        page.click(f'#slist [data-p="{p}"] .sdraw')
        if added and f"−{len(added)} highlight" not in page.inner_text("#tlNote"):
            problems.append(f"removing {p} did not say {len(added)} highlights went")
        if added:
            return problems
    return problems + ["no pool added highlights to check"]


def check_dock(page: Page) -> list[str]:
    """The slim timeline docks while the full strip is out of view, goes
    when it is in view, and dragging on it moves the run."""
    problems = []
    size = page.viewport_size
    page.set_viewport_size({"width": size["width"], "height": 600})  # strip below the fold
    page.evaluate("() => window.scrollTo(0, 0)")
    page.click("#allBtn")
    page.wait_for_timeout(300)
    tops = page.eval_on_selector_all(
        "#pools .pool:not(.focus)", "ps => ps.map(p => Math.round(p.getBoundingClientRect().top))"
    )
    if len(tops) > 1 and len(set(tops)) == len(tops):
        problems.append("the drawn pools are stacked one per row, not side by side")
    full = page.evaluate("document.getElementById('tlCanvas').getBoundingClientRect().bottom")
    if full > page.viewport_size["height"] and not page.evaluate("bpsim.docked()"):
        problems.append("the strip is out of view but the timeline did not dock")
    if page.evaluate("bpsim.docked()"):
        box = page.query_selector("#dockCanvas").bounding_box()
        page.mouse.move(box["x"] + box["width"] * 0.25, box["y"] + 7)
        page.mouse.down()
        page.mouse.move(box["x"] + box["width"] * 0.5, box["y"] + 7)
        page.mouse.up()
        want = int(0.5 * page.evaluate("bpsim.accesses()"))
        if abs(page.evaluate("bpsim.at()") - want) > 1:
            problems.append(
                f"dragging on the docked timeline landed on {page.evaluate('bpsim.at()')}"
            )
    page.locator("#tlCanvas").scroll_into_view_if_needed()
    page.wait_for_timeout(300)
    if page.evaluate("bpsim.docked()"):
        problems.append("the timeline stayed docked with the full strip in view")
    page.click("#allBtn")
    page.evaluate("() => window.scrollTo(0, 0)")
    page.set_viewport_size(size)
    return problems


def check_theme(page: Page, scheme: str) -> list[str]:
    """Follows the system until the button picks; the page, the strip and the
    memory chart switch; the choice survives a reload; nothing is left set."""
    problems = []

    def look() -> tuple:
        bg = page.evaluate("getComputedStyle(document.body).backgroundColor")
        strip = page.evaluate(
            """() => { const c = document.getElementById('tlCanvas');
            const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
            let s = 0; for (let i = 0; i < d.length; i += 97) s += d[i]; return s; }"""
        )
        return page.evaluate("bpsim.theme()"), bg, strip

    first = look()
    if first[0] != scheme:
        problems.append(f"with the system {scheme}, the page is {first[0]}")
    page.click("#themeBtn")
    other = look()
    if other[0] == scheme or other[1] == first[1]:
        problems.append(f"the theme button did not switch the page ({first[:2]} → {other[:2]})")
    if other[2] == first[2]:
        problems.append("the timeline strip was not redrawn in the new theme")
    page.reload()
    page.wait_for_selector(".pool")
    if page.evaluate("bpsim.theme()") != other[0]:
        problems.append("the chosen theme was not remembered")
    page.click("#themeBtn")
    if page.evaluate("bpsim.theme()") != scheme:
        problems.append("a second click did not switch back")
    page.evaluate(
        """() => { localStorage.removeItem('bpsim.theme');
        delete document.documentElement.dataset.theme; }"""
    )
    return problems


def check_motion(page: Page, reduced: bool) -> list[str]:
    """Pages fly in only on a single step into the open pool; never with
    reduce motion; never while playing."""
    problems = []
    kinds = page.evaluate("bpsim.kinds(bpsim.focus())")
    miss = next((i for i, k in enumerate(kinds) if k == "miss" and i > 0), None)
    if miss is not None:
        page.evaluate("k => bpsim.jump(k)", miss - 1)
        page.keyboard.press("ArrowRight")
        page.wait_for_timeout(120)
        flying = len(page.query_selector_all(".fly"))
        if reduced and flying:
            problems.append("a page flew with reduce motion on")
        if not reduced and not flying:
            problems.append("a single step onto a miss did not fly the page in")
    page.evaluate("() => bpsim.jump(-1)")
    page.select_option("#speed", index=2)
    page.click("#play")
    if flies_during(page, 2.0):
        problems.append("pages flew while playing")
    if reduced and page.evaluate("document.getAnimations().length"):
        problems.append("something animated with reduce motion on")
    page.click("#play")
    return problems


SWEEP_ALL = ["--policy", "all", "--frames", "sweep"]
SWEEPS = [
    ("reports sweep, with SQLite's cache",
     ["--trace", str(TRACE_DIR / "reports.trace.json.gz"), *SWEEP_ALL]),
    ("shop sweep", ["--trace", str(TRACE_DIR / "shop.trace.json"), *SWEEP_ALL]),
    ("workload sweep, no LRU, no page size",
     ["--policy", "pg,opt", "--frames", "8,16,64", "--ticks", "300"]),
]  # fmt: skip
ORDER = ["fifo", "clock", "lru", "pg", "innodb", "opt"]  # as the page draws them
DROP = """t => {
    const dt = new DataTransfer();
    dt.items.add(new File([t], 'sweep.json'));
    const init = {dataTransfer: dt, bubbles: true, cancelable: true};
    document.dispatchEvent(new DragEvent('drop', init));
}"""


def decimals(values: list[float]) -> int:
    """As the page picks them: enough for three figures of the largest."""
    return max(0, 2 - math.floor(math.log10(max(values) or 1)))


def plotted(sweep: dict, policy: str, measure: str) -> list[float]:
    by_size = {p["frames"]: p for p in sweep["points"] if p["policy"] == policy}
    if measure == "reads":
        return [by_size[f]["disk_reads"] / sweep["queries"] for f in sweep["sizes"]]
    return [by_size[f][f"{measure}_ms_per_query"] for f in sweep["sizes"]]


def check_sweep_page(page: Page, sweep: dict, measure: str) -> list[str]:
    """The lines plot the sweep's numbers, and the table shows them."""
    problems = []
    if page.evaluate("document.documentElement.scrollWidth > innerWidth"):
        problems.append("the page scrolls sideways")
    policies = [p for p in ORDER if p in sweep["policies"]]
    lines = page.evaluate(
        "[...document.querySelectorAll('#chart polyline')].map(l => l.getAttribute('points'))"
    )
    if len(lines) != len(policies):
        return problems + [f"{len(lines)} lines drawn for {len(policies)} policies"]
    xs, ys, vs = [], [], []
    for policy, line in zip(policies, lines, strict=True):
        pts = [tuple(map(float, xy.split(","))) for xy in line.split()]
        values = plotted(sweep, policy, measure)
        if len(pts) != len(sweep["sizes"]):
            problems.append(f"{policy}: {len(pts)} points for {len(sweep['sizes'])} sizes")
            continue
        xs += [(x, math.log(f)) for (x, _), f in zip(pts, sweep["sizes"], strict=True)]
        ys += [y for _, y in pts]
        vs += values
    # y must be a straight-line function of the value, x of the log of the size
    for name, pairs in [("height", list(zip(vs, ys, strict=True))), ("position", xs)]:
        (a0, b0), (a1, b1) = min(pairs), max(pairs)
        off = max(abs(b0 + (a - a0) * (b1 - b0) / (a1 - a0) - b) for a, b in pairs)
        if off > 0.5:
            problems.append(f"a point's {name} is {off:.1f}px off a faithful plot")
    dots = len(page.query_selector_all("#chart circle.sq"))
    sizes = sweep["sizes"]
    in_range = sum(sizes[0] <= d["pages"] <= sizes[-1] for d in sweep["sqlite_cache"])
    want = in_range if measure == "reads" else 0
    if dots != want:
        problems.append(f"{dots} SQLite dots on the {measure} chart, expected {want}")
    cells = page.evaluate(
        """[...document.querySelectorAll('#table tbody tr')]
            .map(r => [...r.cells].map(c => c.textContent))"""
    )
    every = [v for p in policies for v in plotted(sweep, p, measure)]
    d = decimals(every)
    first = 2 if sweep.get("page_size") else 1
    for row, f in zip(cells, sizes, strict=True):
        for cell, policy in zip(row[first : first + len(policies)], policies, strict=True):
            want_v = plotted(sweep, policy, measure)[sizes.index(f)]
            if abs(float(cell.replace(",", "")) - want_v) > 0.5 * 10**-d + 1e-9:
                problems.append(f"table, {policy} at {f}: {cell}, expected {want_v:.{d + 2}f}")
    return problems


def readout(page: Page) -> list[dict]:
    return page.evaluate(
        """[...document.querySelectorAll('#readout .row')].map(r => ({
            name: r.children[1].textContent, value: r.querySelector('.val').textContent,
            vs: r.querySelector('.vs')?.textContent ?? null}))"""
    )


def lru_disk_reads(trace: Path, frames: int, queries: int, work: Path) -> float:
    """Pages per query the engine's LRU reads at `frames`, read-ahead off."""
    out = work / f"lru-{frames}"
    cmd = [str(compare.SIM), "--trace", str(trace), "--policy", "lru", "--frames", str(frames)]
    cmd += ["--read-ahead", "off", "--no-viewer", "--out", str(out)]
    subprocess.run(cmd, check=True, capture_output=True)
    summary = json.loads(next(out.glob("**/summary.json")).read_text(encoding="utf-8"))
    return (summary["misses"] - summary["fresh_pages"]) / queries


def check_sweep_readout(page: Page, sweep: dict, trace: Path, work: Path) -> list[str]:
    """Opens near half the database. Where a policy beats LRU, the range of
    memory it says LRU would need holds when the engine runs LRU at both ends:
    still more reads at the low end, no more at the high end."""
    problems = []
    sizes = sweep["sizes"]
    half = min(range(len(sizes)), key=lambda i: abs(sizes[i] - sweep["pages"] / 2))
    if page.evaluate("bpsim.at()") != half:
        problems.append(f"the readout opened at size {page.evaluate('bpsim.size()')}")
    names = {"PostgreSQL-style": "pg", "InnoDB-style": "innodb", "Clock": "clock", "FIFO": "fifo"}
    lru = plotted(sweep, "lru", "reads")[half]
    nearest = lambda ratio: min(sizes, key=lambda f: abs(f - ratio * sizes[half]))  # noqa: E731
    claims = 0
    for row in readout(page):
        policy = names.get(row["name"])
        if policy is None:
            continue
        target = plotted(sweep, policy, "reads")[half]
        if target < lru and row["vs"] is None:
            problems.append(f"{row['name']} beats LRU but the readout says nothing")
        if row["vs"] is None:
            continue
        claims += 1
        ratios = row["vs"].split("need ")[1].split("×")[0].removeprefix("up to ").split("–")
        ends = [nearest(float(r)) for r in ratios]
        if len(ends) == 2 and lru_disk_reads(trace, ends[0], sweep["queries"], work) <= target:
            problems.append(f"LRU already matches {row['name']} at {ends[0]} frames")
        if lru_disk_reads(trace, ends[-1], sweep["queries"], work) > target + 1e-12:
            problems.append(f"LRU at {ends[-1]} frames still reads more than {row['name']}")
    if not claims:
        problems.append("no policy showed how much memory LRU would need")
    page.focus("#chart")
    page.keyboard.press("ArrowRight")
    if page.evaluate("bpsim.at()") != half + 1:
        problems.append("the right arrow did not move to the next size")
    page.keyboard.press("Home")
    shown = page.inner_text("#readout")
    if page.evaluate("bpsim.size()") != sizes[0] or f"{sizes[0]} frames" not in shown:
        problems.append("Home did not move the readout to the smallest size")
    page.keyboard.press("End")
    if page.evaluate("bpsim.size()") != sizes[-1]:
        problems.append("End did not move to the largest size")
    box = page.query_selector("#chart").bounding_box()
    page.mouse.move(box["x"] + 50, box["y"] + box["height"] / 2)
    if page.evaluate("bpsim.at()") != 0:
        problems.append("pointing at the left of the chart did not pick the smallest size")
    return problems


def check_sweep_controls(page: Page, sweep: dict, scheme: str) -> list[str]:
    """Time on each drive; the storage switch only for time; the theme."""
    problems = []
    page.click("#mTime")
    if not page.is_visible("#storageSeg"):
        problems.append("the storage switch is hidden in the time view")
    problems += [f"time, SSD: {p}" for p in check_sweep_page(page, sweep, "ssd")]
    if "(the floor)" in page.inner_text("#legend"):
        problems.append("Optimal is called the floor in the time view")
    page.click("#sHdd")
    problems += [f"time, HDD: {p}" for p in check_sweep_page(page, sweep, "hdd")]
    page.click("#mReads")
    if page.is_visible("#storageSeg"):
        problems.append("the storage switch shows in the disk reads view")
    before = page.evaluate("getComputedStyle(document.body).backgroundColor")
    page.click("#themeBtn")
    if (
        page.evaluate("bpsim.theme()") == scheme
        or page.evaluate("getComputedStyle(document.body).backgroundColor") == before
    ):
        problems.append("the theme button did not switch the page")
    page.reload()
    page.wait_for_selector("#chart polyline")
    if page.evaluate("bpsim.theme()") == scheme:
        problems.append("the chosen theme was not remembered")
    page.evaluate(
        """() => { localStorage.removeItem('bpsim.theme');
        delete document.documentElement.dataset.theme; }"""
    )
    return problems


def check_sweep_drop(page: Page, sweep: dict) -> list[str]:
    """The page as shipped waits for a file, and refuses one of another version."""
    page.goto((compare.REPO / "viewer" / "sweep.html").as_uri())
    if not page.is_visible("#drop"):
        return ["the empty page does not ask for a sweep.json"]
    newer = dict(sweep, version=sweep["version"] + 1)
    page.evaluate(DROP, json.dumps(newer))
    page.wait_for_timeout(200)
    if "this page reads version" not in page.inner_text("#error"):
        return ["a sweep of another version was not refused with a clear error"]
    page.evaluate(DROP, json.dumps(sweep))
    page.wait_for_selector("#chart polyline")
    return []


def check_sweeps(browser, work: Path) -> tuple[int, list[str]]:  # noqa: ANN001
    problems, checked = [], 0
    for i, (label, args) in enumerate(SWEEPS):
        out = work / f"sweep{i}"
        cmd = [str(compare.SIM), *args, "--out", str(out)]
        subprocess.run(cmd, check=True, capture_output=True)
        html = next(out.glob("**/sweep.html"))
        sweep = json.loads((html.parent / "sweep.json").read_text(encoding="utf-8"))
        for width, scheme in [(1280, "light"), (400, "dark")]:
            page = browser.new_page(viewport={"width": width, "height": 1000}, color_scheme=scheme)
            errors: list[str] = []
            page.on("pageerror", lambda e, errors=errors: errors.append(str(e)))
            page.goto(html.as_uri())
            page.wait_for_selector("#chart polyline")
            found = check_sweep_page(page, sweep, "reads")
            if i == 0:
                found += check_sweep_controls(page, sweep, scheme)
                if width == 1280:
                    trace = Path(args[1])
                    found += check_sweep_readout(page, sweep, trace, work)
                    found += check_sweep_drop(page, sweep)
            found += [f"script error: {e}" for e in errors]
            problems += [f"{label} ({width}px {scheme}): {p}" for p in found]
            checked += 1
            page.close()
    return checked, problems


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
                if i == 0:
                    found += check_how(page)
                    found += check_theme(page, scheme)
                if width == 1280 and i == 0:
                    found += check_mem(page, run)
                    found += check_sound(page)
                    found += check_changes(page)
                    found += check_highlights(page)
                    found += check_race_changes(page)
                    found += check_dock(page)
                    found += check_drawn(page)
                    found += check_motion(page, reduced=False)
                    found += check_interaction(page, viewer.parent / "run.json")
                found += [f"script error: {e}" for e in errors]
                problems += [f"{label} ({width}px {scheme}): {p}" for p in found]
                checked += 1
                page.close()
            if i == 0:
                page = browser.new_page(
                    viewport={"width": 1280, "height": 1000}, reduced_motion="reduce"
                )
                page.goto(viewer.as_uri())
                page.wait_for_selector(".pool")
                found = check_motion(page, reduced=True)
                problems += [f"{label} (reduce motion): {p}" for p in found]
                checked += 1
                page.close()
        sweep_pages, sweep_problems = check_sweeps(browser, work)
        checked += sweep_pages
        problems += sweep_problems
        browser.close()
    for p in problems:
        print("PROBLEM", p)
    print(f"{checked} pages checked, {len(problems)} problems")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
