-- Pull action items out of the open document.

if not evo.doc.is_open() then
  evo.log("Open a document first.")
  return
end

local text = evo.doc.text()
if #text == 0 then
  evo.log("No text could be read from this document.")
  return
end

local items = evo.model.generate(
  "List every action item, task, deadline and commitment in this document. " ..
  "Give one per line, each naming who is responsible and when it is due if " ..
  "the document says. If there are none, say so plainly.\n\n" .. text,
  {
    system = "Extract only what the document actually states. Do not infer " ..
             "tasks that are not there.",
    temperature = 0.0,
  }
)

evo.create_document("Action items — " .. evo.doc.title(), items)
