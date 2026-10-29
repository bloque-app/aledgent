#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""One tar layer holding a core code set, the same bytes on every machine.

    tools/release/pack-code-layer.py <set-dir> <out.tar> [<set.json>]

<set-dir> is what `tools/release/core-code-set.sh` wrote: regular files named by their blake2b-256
digest and nothing else. The layer holds them under `code/`, sorted by name, owner 0, mode 0444, time
0, with no extended attributes, so that the layer, and the image `crane append` makes of it, have a
digest that follows from the files alone. Anything in the directory that is not a file named by
64 lowercase hex digits is refused. With <set.json> (core-code-set.sh's record) the layer also holds it
as `set.json`, beside `code/`, so that a later release can read what the set is; the `fs` source reads
`code/` and never sees it.
"""

import re
import sys
import tarfile
from pathlib import Path

NAME = re.compile(r"^[0-9a-f]{64}$")


def main(argv: list[str]) -> int:
    if len(argv) not in (3, 4):
        print(__doc__, file=sys.stderr)
        return 2
    src, out = Path(argv[1]), Path(argv[2])
    record = Path(argv[3]) if len(argv) == 4 else None
    if record is not None and (record.is_symlink() or not record.is_file()):
        print(f"pack-code-layer: {record} is not a regular file", file=sys.stderr)
        return 1
    files = sorted(src.iterdir(), key=lambda p: p.name)
    if not files:
        print(f"pack-code-layer: {src} is empty", file=sys.stderr)
        return 1
    for f in files:
        if not NAME.match(f.name) or f.is_symlink() or not f.is_file():
            print(f"pack-code-layer: {f} is not a regular file named by a digest", file=sys.stderr)
            return 1
    with tarfile.open(out, "w", format=tarfile.USTAR_FORMAT) as tar:
        d = tarfile.TarInfo("code")
        d.type, d.mode, d.mtime, d.uid, d.gid, d.uname, d.gname = tarfile.DIRTYPE, 0o555, 0, 0, 0, "", ""
        tar.addfile(d)
        for f in files:
            info = tarfile.TarInfo(f"code/{f.name}")
            info.size = f.stat().st_size
            info.mode, info.mtime, info.uid, info.gid, info.uname, info.gname = 0o444, 0, 0, 0, "", ""
            with f.open("rb") as fh:
                tar.addfile(info, fh)
        if record is not None:
            info = tarfile.TarInfo("set.json")
            info.size = record.stat().st_size
            info.mode, info.mtime, info.uid, info.gid, info.uname, info.gname = 0o444, 0, 0, 0, "", ""
            with record.open("rb") as fh:
                tar.addfile(info, fh)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
