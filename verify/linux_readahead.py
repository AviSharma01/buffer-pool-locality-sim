"""Watch real Linux read-ahead, to compare with the engine's rule (Linux only).

    python verify/linux_readahead.py

Writes an 8 MB file to the system temp folder, then for each pattern: evicts the file
from the page cache (posix_fadvise DONTNEED, no root needed), reads a few 4 KB
pages with pread, and asks the kernel which pages are now cached (mincore).

The engine reads ahead on a miss that continues a table (the second page in a
row) and never for scattered or strided reads. On a 6.18 kernel with ext4 and
128 KB read-ahead (2026-11-18): p1000 then p1001 cached p1000-p1004, while one
page, scattered pages and every third page cached only what was read. Linux
also reads ahead on the very first read at the start of a file, and grows its
window as a stream continues; the engine does neither. Informational: it
prints what this machine does and does not fail.
"""

from __future__ import annotations

import ctypes
import mmap
import os
import platform
import sys
import tempfile
from pathlib import Path

PAGE = 4096
PAGES = 2048  # 8 MB
FILE = Path(tempfile.gettempdir()) / "bpsim-readahead-probe.bin"
PATTERNS = [
    ("one page at the start of the file", [0]),
    ("one page in the middle", [1000]),
    ("two pages in a row", [1000, 1001]),
    ("four pages in a row", [1000, 1001, 1002, 1003]),
    ("scattered pages", [100, 700, 1300]),
    ("every third page", [1000, 1003, 1006, 1009]),
]


def cached_pages(fd: int, libc: ctypes.CDLL) -> list[int]:
    addr = libc.mmap(None, PAGE * PAGES, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
    flags = (ctypes.c_ubyte * PAGES)()
    if libc.mincore(addr, PAGE * PAGES, flags) != 0:
        raise OSError(ctypes.get_errno(), "mincore failed")
    libc.munmap(addr, PAGE * PAGES)
    return [i for i in range(PAGES) if flags[i] & 1]


def runs(pages: list[int]) -> str:
    out, start = [], None
    for i, p in enumerate(pages):
        start = p if start is None else start
        if i + 1 == len(pages) or pages[i + 1] != p + 1:
            out.append(f"p{start}-p{p}" if p > start else f"p{p}")
            start = None
    return ", ".join(out)


def main() -> int:
    if platform.system() != "Linux":
        print(f"Linux only (this is {platform.system()}); macOS reads ahead differently.")
        return 0
    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_long,
    ]
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.POINTER(ctypes.c_ubyte)]
    with open(FILE, "wb") as f:
        f.write(os.urandom(PAGE * PAGES))
        f.flush()
        os.fsync(f.fileno())  # dirty pages cannot be evicted until written out
    print(f"kernel {platform.release()}")
    try:
        for name, reads in PATTERNS:
            fd = os.open(FILE, os.O_RDONLY)
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
            before = len(cached_pages(fd, libc))
            for p in reads:
                os.pread(fd, PAGE, p * PAGE)
            after = cached_pages(fd, libc)
            os.close(fd)
            read = ", ".join(f"p{p}" for p in reads)
            print(f"{name:34} read {read:28} cached {len(after):2} (was {before}): {runs(after)}")
    finally:
        FILE.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
