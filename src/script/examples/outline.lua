-- Build a section-by-section outline of the open document.
--
-- Works page by page rather than in one pass, which keeps each prompt small
-- enough for a modest local model to handle.

if not evo.doc.is_open() then
  evo.log("Open a document first.")
  return
end

local parts = {}
local pages = evo.doc.page_count()

for page = 1, pages do
  local text = evo.doc.text(page)
  if #text > 0 then
    evo.log("Page " .. page .. " of " .. pages .. "…")
    local note = evo.model.generate(
      "In one or two sentences, say what this page covers.\n\n" .. text,
      { temperature = 0.1, max_tokens = 200 }
    )
    table.insert(parts, "Page " .. page .. "\n" .. note)
  end
end

if #parts == 0 then
  evo.log("No text found to outline.")
  return
end

evo.create_document("Outline of " .. evo.doc.title(), table.concat(parts, "\n\n"))
