#!/usr/bin/env python3
"""Take the README's screenshots: the sweep page and the viewer.

    python verify/screenshots.py    # writes docs/sweep.png and docs/viewer.png

Both come from the shipped traces at a fixed size in light mode, so running
this again after the pages change gives comparable pictures. Needs Playwright.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compare  # noqa: E402

DOCS = compare.REPO / "docs"
TRACES = compare.REPO / "traces"
WIDTH = 1280


def page_from(sim: Path, args: list[str], out: Path, name: str) -> Path:
    """Run the engine and return the page it wrote."""
    subprocess.run([str(sim), *args, "--out", str(out)], check=True, capture_output=True)
    return next(out.glob(f"**/{name}"))


def main() -> int:
    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        print("Playwright is missing: pip install -r verify/requirements.txt")
        return 1
    sim = compare.build()
    DOCS.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp, sync_playwright() as pw:
        out = Path(tmp)
        sweep = page_from(
            sim,
            [
                "--trace",
                str(TRACES / "reports.trace.json.gz"),
                "--policy",
                "all",
                "--frames",
                "sweep",
            ],
            out / "sweep",
            "sweep.html",
        )
        viewer = page_from(
            sim,
            ["--trace", str(TRACES / "shop.trace.json"), "--policy", "all", "--frames", "8"],
            out / "viewer",
            "viewer.html",
        )
        browser = pw.chromium.launch()
        page = browser.new_page(
            viewport={"width": WIDTH, "height": 900}, color_scheme="light", device_scale_factor=2
        )
        page.goto(sweep.as_uri())
        page.wait_for_selector("#chart polyline")
        page.evaluate("document.fonts.ready.then(() => true)")
        clip = page.evaluate(
            """() => {
            const a = document.querySelector('.top').getBoundingClientRect();
            const b = document.querySelector('section.panel').getBoundingClientRect();
            const pad = 20;
            return {x: b.left - pad, y: a.top - pad, width: b.width + 2 * pad,
                    height: b.bottom - a.top + 2 * pad};
        }"""
        )
        page.screenshot(path=DOCS / "sweep.png", clip=clip, full_page=True)
        page.goto(viewer.as_uri())
        page.wait_for_function("() => window.bpsim && bpsim.accesses() > 0")
        page.evaluate("bpsim.jump(Math.floor(bpsim.accesses() * 0.6))")
        page.evaluate("document.fonts.ready.then(() => true)")
        page.wait_for_timeout(300)  # the jump's transition
        page.screenshot(path=DOCS / "viewer.png")
        browser.close()
    for name in ("sweep.png", "viewer.png"):
        print(f"wrote docs/{name} ({(DOCS / name).stat().st_size // 1024} KB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
