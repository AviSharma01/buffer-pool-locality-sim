# verify

Checks that do not trust the engine's own tests. Run them from the repo root.
`compare.py` and `fuzz.py` build the release engine first (`cargo build --release`), so they
never check a stale binary; `browser.py` needs it built.

| Command | What it checks | Time |
|---|---|---|
| `python verify/compare.py` | The engine against an independent model (`model.py`) on every shipped trace and a free-page scenario: every policy, pool sizes 1–32, SSD and HDD, read-ahead auto/off/1/3. Then a sweep: every policy at 512 frames on the reports workload trace, disk reads with read-ahead off and time on both drives with it on. Also runs inside `pytest` from `capture/`. | ~40 s |
| `python verify/fuzz.py` | The engine against the model on random small databases (objects in runs of pages, free pages, walks, jumps, writes, random scans): every policy, pool sizes 1–16, both drives, read-ahead auto/off/3. `--seed`, `--traces`, `--keep DIR` for failing traces. A short run is in `pytest`. | ~1 min |
| `python verify/browser.py` | The viewer in a real browser, including a 2,500-request run: desktop and phone, light and dark; no errors or overflow; every access on both drives; times and the score column's hits equal the model's; every pool opened, all pools shown, a pool drawn beside the open one and raced against it; disk marks and the timeline strip drawn; "How it works": every walkthrough step, Esc, focus returning, the hint remembered; pages fly in only on a single step (never while playing, never with reduce motion); memory over time closed at first, its bars matching the run file, pointing and dragging; sound off and silent by default, the rising pop as a test, one pop for a burst of pool changes, the falling pop on removal, silence while stepping, the win at a race's end, the choice remembered; moments naming only pools on screen and changing when one is drawn, Play going on through a change, a drawn pool's name swapping it to large, the 2-second glow; highlights a change adds glowing and counted in the note, then cleared; a race carrying on through a draw, a removal and a swap, a mid-race arrival joining near the others, places for exactly the pools in it; drawn pools side by side; the timeline docking while the strip is out of view (and dragging on it), undocking when it's in view; the theme following the system until the button picks, switching the page and the strip, and remembered; race, keys, dragging on the timeline, and refusing an old run file. Then the sweep page, for a sweep with SQLite's own cache measured, a small one and a workload's: the lines a faithful plot of the sweep's numbers and the table showing them, on both drives; the readout opening near half the database, each policy against LRU and the top line matching numbers worked out in Python, and "LRU needs N× the memory" being the smallest pool where the engine's LRU reads no more; keys and pointing; the storage switch only for time; the theme; refusing a sweep of another version. Needs Playwright. | ~2 min |
| `python verify/findings.py --check` | FINDINGS.md and the README's headline: captures the trace variants into `traces/local/variants` the first time (~4 min), measures every policy on them, and fails if a claim no longer holds or a number in the doc is out of date. Without `--check` it rewrites the doc's numbers. Its claim checks are tested in `capture/tests/test_findings.py`. | ~30 s |
| `python verify/linux_readahead.py` | What real Linux read-ahead does on this machine, to compare with the engine's rule. Linux only; informational. | ~1 s |
| `python verify/screenshots.py` | Not a check: retakes `docs/sweep.png` and `docs/viewer.png` for the README. Needs Playwright. | ~10 s |

`model.py` was written from a description of the rules, not from the engine's
code (the policies by a reviewer that hadn't seen it). It shows the engine
follows the rules; whether the rules match real systems rests on the datasheets
cited in `sim/src/storage.rs` and on `linux_readahead.py`.

Playwright, once: `pip install -r verify/requirements.txt && python -m playwright install chromium`
