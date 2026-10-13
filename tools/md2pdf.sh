#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Usage: tools/md2pdf.sh <input.md> [output.pdf]
# Markdown -> HTML (pandoc, mermaid fences kept live) -> PDF (headless Chrome, diagrams rendered).
set -euo pipefail
IN="$1"; OUT="${2:-${IN%.md}.pdf}"
HERE="$(cd "$(dirname "$0")" && pwd)"
TMP="$(mktemp -d)"; HTML="$TMP/doc.html"
CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
HEAD="$TMP/head.html"
cat > "$HEAD" <<'H'
<style>$CSS$</style>
<script type="module">
  import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
  mermaid.initialize({ startOnLoad: false, theme: "neutral", securityLevel: "loose",
    themeVariables: { fontSize: "22px", fontFamily: "Helvetica, Arial, sans-serif" },
    flowchart: { useMaxWidth: true, htmlLabels: true, nodeSpacing: 40, rankSpacing: 50, curve: "basis", padding: 12 } });
  // Render sequentially with explicit ids: mermaid's default ids are time-based and collide
  // under headless virtual time when a page holds more than one diagram.
  let k = 0;
  for (const el of document.querySelectorAll("pre.mermaid")) {
    const { svg } = await mermaid.render("mmd-" + (k++), el.textContent);
    el.innerHTML = svg;
  }
  document.documentElement.setAttribute("data-mermaid-done", "1");
</script>
H
# inline the CSS
python3 - "$HEAD" "$HERE/print.css" <<'PY'
import sys; h=open(sys.argv[1]).read(); c=open(sys.argv[2]).read(); open(sys.argv[1],'w').write(h.replace('$CSS$',c))
PY
pandoc "$IN" -f gfm+yaml_metadata_block -t html5 --standalone --toc --toc-depth=2 \
  --lua-filter "$HERE/mermaid.lua" --include-in-header "$HEAD" \
  --metadata pagetitle="$(basename "${IN%.md}")" -o "$HTML"
# Render the YAML front matter (pandoc drops unknown keys) as a small block after the H1.
python3 - "$IN" "$HTML" <<'PY'
import sys,re
md=open(sys.argv[1]).read(); html=open(sys.argv[2]).read()
m=re.match(r'^---\n(.*?)\n---\n',md,re.S)
if m:
    items=[l.split(':',1) for l in m.group(1).splitlines() if ':' in l]
    block='<dl class="frontmatter">'+''.join(f'<dt>{k.strip()}</dt><dd>{v.strip()}</dd>' for k,v in items)+'</dl>'
    html=re.sub(r'(</h1>)',r'\1'+block,html,count=1)
    open(sys.argv[2],'w').write(html)
PY
"$CHROME" --headless=new --disable-gpu --no-sandbox --run-all-compositor-stages-before-draw \
  --virtual-time-budget=30000 --no-pdf-header-footer --print-to-pdf="$OUT" "file://$HTML" 2>/dev/null
echo "wrote $OUT"; rm -rf "$TMP"
