-- Summarize the open document into a new one.
--
-- Every script runs with an `evo` table available:
--
--   evo.log(message)                  write a line to the Scripts log
--   evo.doc.is_open()                 is a document open?
--   evo.doc.title()                   its title
--   evo.doc.page_count()              how many pages
--   evo.doc.text([page])              all its text, or one page's (1-based)
--   evo.model.generate(prompt, opts)  ask the local model; returns a string
--                                     opts: system, temperature, max_tokens, model
--   evo.model.list()                  model names the endpoint offers
--   evo.create_document(title, text)  add a new PDF to your library
--
-- The model is whatever you configured in Preferences > Scripting.

if not evo.doc.is_open() then
  evo.log("Open a document first.")
  return
end

local text = evo.doc.text()
if #text == 0 then
  evo.log("No text could be read from this document. Scanned pages need OCR first.")
  return
end

evo.log("Summarizing " .. evo.doc.page_count() .. " pages…")

local summary = evo.model.generate(
  "Summarize the following document. Open with a one-paragraph overview, " ..
  "then give the key points as a short list.\n\n" .. text,
  {
    system = "You are a careful technical editor. Be concise and factual, " ..
             "and never invent detail that isn't in the source.",
    temperature = 0.2,
  }
)

evo.create_document("Summary of " .. evo.doc.title(), summary)
