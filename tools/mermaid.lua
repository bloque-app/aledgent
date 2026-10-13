-- Copyright (c) 2026 Pagos Cube LLC
-- SPDX-License-Identifier: Apache-2.0 OR MIT
-- Turn ```mermaid fences into <pre class="mermaid"> so mermaid.js renders them in the browser.
function CodeBlock(el)
  if el.classes:includes("mermaid") then
    local esc = el.text:gsub("&","&amp;"):gsub("<","&lt;"):gsub(">","&gt;")
    return pandoc.RawBlock("html", '<pre class="mermaid">' .. esc .. '</pre>')
  end
end
