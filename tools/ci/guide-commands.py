#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""A guide page's commands, as one Bash script (F-039, M-3D's walkthrough).

    tools/ci/guide-commands.py site/src/content/docs/operate/one-machine.mdx > walkthrough.sh

Every fenced block whose language is `sh` is a command the reader runs: they are written out in the
page's order, in one script, run in one shell from the repository's root, stopping at the first
failure. A block the reader should not run as it stands (a placeholder, a clone, an example output)
takes another language (`text`, `console`, `json`). So a command cannot be on the page and not be
run by CI, and CI runs nothing that is not on the page.

A block's fence may be indented (inside a list, or `<Steps>`); its indentation is taken off every
line. Each block is announced on stderr with its line, so a failure names the step.
"""

import re
import sys

FENCE = re.compile(r"^(?P<indent>[ \t]*)(?P<fence>`{3,}|~{3,})(?P<info>.*)$")


def blocks(text):
    """(line, lang, body) of every fenced block, in order."""
    lines = text.split("\n")
    i = 0
    while i < len(lines):
        m = FENCE.match(lines[i])
        if not m:
            i += 1
            continue
        indent, fence = m.group("indent"), m.group("fence")
        lang = (m.group("info").strip().split() or [""])[0]
        start = i + 1
        body = []
        i += 1
        while i < len(lines):
            line = lines[i]
            if line.strip().startswith(fence) and line.strip().strip(fence[0]) == "":
                break
            if line.startswith(indent):
                line = line[len(indent):]
            elif line.strip():
                raise SystemExit(f"line {i + 1}: a line of the block is indented less than its fence")
            body.append(line)
            i += 1
        else:
            raise SystemExit(f"line {start}: the block is not closed")
        yield start, lang, "\n".join(body)
        i += 1


def main():
    if len(sys.argv) != 2:
        raise SystemExit("usage: guide-commands.py <page.md|page.mdx>")
    path = sys.argv[1]
    with open(path, encoding="utf-8") as f:
        text = f.read()
    out = [
        "#!/usr/bin/env bash",
        f"# Generated from {path} by tools/ci/guide-commands.py: every `sh` block, in order.",
        "set -euo pipefail",
    ]
    n = 0
    for line, lang, body in blocks(text):
        if lang != "sh":
            continue
        n += 1
        out.append("")
        out.append(f"# --- block {n}, {path}:{line}")
        out.append(f"echo '[guide] block {n} ({path}:{line})' >&2")
        out.append(body)
    if n == 0:
        raise SystemExit(f"{path}: no sh block")
    print("\n".join(out))


if __name__ == "__main__":
    main()
