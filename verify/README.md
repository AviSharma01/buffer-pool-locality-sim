# verify

Checks that do not trust the engine's own tests. Run them from the repo root.
`compare.py` and `fuzz.py` build the release engine first (`cargo build --release`), so they
never check a stale binary; `browser.py` needs it built.

| Command | What it checks | Time |
|---|---|---|
| `python verify/compare.py` | The engine against an independent model (`model.py`) on every shipped trace and a free-page scenario: every policy, pool sizes 1–32, SSD and HDD, read-ahead auto/off/1/3. Also runs inside `pytest` from `capture/`. | ~10 s |
| `python verify/fuzz.py` | The engine against the model on random small databases (objects in runs of pages, free pages, walks, jumps, writes, random scans): every policy, pool sizes 1–16, both drives, read-ahead auto/off/3. `--seed`, `--traces`, `--keep DIR` for failing traces. A short run is in `pytest`. | ~1 min |
| `python verify/browser.py` | The viewer in a real browser, including a 2,500-request run: desktop and phone, light and dark; no errors or overflow; every access on both drives; times and the score column's hits equal the model's; every pool opened, all pools shown; disk marks and the timeline strip drawn; race, keys, dragging on the timeline, and refusing an old run file. Needs Playwright. | ~1 min |
| `python verify/linux_readahead.py` | What real Linux read-ahead does on this machine, to compare with the engine's rule. Linux only; informational. | ~1 s |

`model.py` was written from the rules in CLAUDE.md, not from the engine's
code: the policies by a separate reviewer, the pricing and read-ahead rules
from the step plans. It shows the engine does what the plans say; whether the
plans match real systems rests on the sources in CLAUDE.md.

Playwright, once: `pip install -r verify/requirements.txt && python -m playwright install chromium`
