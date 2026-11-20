# verify

Checks that do not trust the engine's own tests. Run them from the repo root
after building the engine (`cd sim && cargo build --release`).

| Command | What it checks | Time |
|---|---|---|
| `python verify/compare.py` | The engine against an independent model (`model.py`) on every shipped trace and a free-page scenario: every policy, pool sizes 1–32, SSD and HDD, read-ahead auto/off/1/3. Also runs inside `pytest` from `capture/`. | ~10 s |
| `python verify/browser.py` | The viewer in a real browser: desktop and phone, light and dark; no errors or overflow; every access on both drives; totals equal the model's; race, keys, dragging, and refusing an old run file. Needs Playwright. | ~1 min |
| `python verify/linux_readahead.py` | What real Linux read-ahead does on this machine, to compare with the engine's rule. Linux only; informational. | ~1 s |

`model.py` was written from the rules in CLAUDE.md, not from the engine's
code: the policies by a separate reviewer, the pricing and read-ahead rules
from the step plans. It shows the engine does what the plans say; whether the
plans match real systems rests on the sources in CLAUDE.md.

Playwright, once: `pip install -r verify/requirements.txt && python -m playwright install chromium`
