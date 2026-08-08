# evo

A PDF editor in Rust — Preview-like viewing with Bluebeam-style precision
markup, a searchable document library, and a local AI assistant.

evo aims for the feel of macOS Preview (open, scroll, zoom, mark up, print)
combined with the thing Preview never gives you: **exact control over your
markups** — numeric X/Y/W/H editing, snapping with alignment guides, and
one-click centering on the page.

**Pure-Rust PDF pipeline.** No PDFium, no MuPDF, no native PDF library at all:
rendering is [hayro](https://github.com/LaurenzV/hayro), editing/writing is
[lopdf](https://github.com/J-F-Liu/lopdf), the UI is
[egui](https://github.com/emilk/egui). (Scripting embeds Lua 5.4 and the
optional built-in model runs on [llama.cpp](https://github.com/ggml-org/llama.cpp) —
the only non-Rust components, both vendored.)

## Features

**Viewing**
- Continuous vertical page layout with smooth pan and zoom (pinch, ⌘+/⌘−,
  fit-width, actual size)
- Background rendering with progressive quality — stays responsive on large
  documents
- Page thumbnails sidebar

**Interface**
- A **ribbon** of grouped controls — history, tools, style, zoom — laid out
  either side of an **evo** button that takes you back to your library.
  Right-click it to hide groups or to drag groups and buttons into the order
  you want; the layout is remembered
- **Rebindable keyboard shortcuts**: every command is listed in
  Preferences (⌘,), where you can record a new chord for it. Conflicts are
  reported rather than silently applied

**Markup** — <kbd>V</kbd> select, <kbd>H</kbd> highlight, <kbd>T</kbd> text,
<kbd>R</kbd> rectangle, <kbd>O</kbd> ellipse, <kbd>L</kbd> line,
<kbd>A</kbd> arrow, <kbd>P</kbd> pen, <kbd>G</kbd> pan (or hold space)
- Every markup has 8 resize handles (shift = lock aspect), drag to move,
  arrow keys to nudge (shift = 10 pt)
- **Snapping & guides**: dragged edges and centers snap to the page center,
  page edges, and other markups, with live alignment guides (hold ⌘ to
  disable)
- **Inspector panel**: type exact X / Y / W / H in points, one-click
  **Center Horizontally** / **Center Vertically** on the page
- Stroke/fill color, stroke width, opacity, font size, text alignment
- Full undo/redo (⌘Z / ⇧⌘Z)

**Pages**
- Multi-select on the thumbnail rail (click / shift / ⌘), then rotate,
  delete, copy/paste (duplicate), print just the selection, or extract
  pages to a new PDF — all undoable; applied on export
- **Combine / Insert PDFs** (File menu, or drop several files onto the
  window): one wizard for building a document out of several. Each file is
  checked as you add it, rows are reordered by dragging, and the result
  either extends the open document or becomes a new one

**Library & search**
- A personal document library (shown when nothing is open): import PDFs,
  browse cards with thumbnails, and search **titles and full page text**
  with highlighted snippets — a result click opens the document at that page
- Text is extracted directly from each PDF; pages with no text layer
  (scans) are OCR'd with the pure-Rust [ocrs](https://github.com/robertknight/ocrs)
  engine. Markup on library documents is saved automatically as a sidecar —
  the original PDF bytes are never modified
- Stored under the platform data directory (`~/Library/Application Support/evo`
  on macOS, `%APPDATA%` on Windows, `~/.local/share/evo` on Linux)

**Assistant**
- **Chat with your document** (⌘⇧C): ask questions about the open PDF in a
  side panel. Answers are grounded in the pages (retrieved per question),
  stream in live, and cite pages as clickable `[p.3]` links. Chat history is
  kept with library documents
- **A built-in local model**: download Qwen3-4B-Instruct (~2.5 GB, Apache-2.0)
  from Preferences ▸ Model — inference runs entirely on your machine via
  llama.cpp. Or point evo at Ollama / LM Studio / any OpenAI-compatible server
  instead. Nothing leaves your computer either way
- **Summaries & auto-tags** (opt-in): imported documents are summarized and
  tagged in the background; both appear on library cards and are searchable

**MCP**
- **evo is an MCP server**: enable it in Preferences ▸ MCP and agents like
  Claude can search your library, read page text, open documents, add markup,
  and export — over localhost HTTP with a bearer token. `evo mcp-serve` also
  offers a headless stdio mode for library access
- **evo is an MCP client**: configure external MCP servers and the chat
  assistant can use their tools (off by default, per-panel toggle). Lua
  scripts can too — but only when you tick "Allow MCP" for that run

**Scripting** — Tools ▸ Scripts
- Embedded **Lua**, with an `evo` table for reading the open document's text
  and asking a **local language model** to write something from it. The
  result is laid out as a PDF and added to your library
- Uses the same model as chat (built-in or your own server, Preferences ▸
  Model). Nothing is sent anywhere else
- Scripts are sandboxed — no filesystem, no processes, no other network
  access — and are stopped by a Cancel button or a configurable time limit.
  Three worked examples are written into the scripts folder on first use

**Output**
- **Export PDF**: markups are written as real PDF annotations with appearance
  streams — they show up (and stay selectable) in Preview, Acrobat, and
  everything else. Optionally flatten them into the page content instead.
  The original page content passes through byte-for-byte untouched.
- **Export SVG**: one SVG per page (original content + markup vectors)
- **Print**: hands a flattened copy to the system print dialog (⌘P)

## Install & run

Requires Rust 1.92+.

```sh
git clone https://github.com/andredegrenier/evo
cd evo
cargo run --release            # then ⌘O or drop a PDF on the window
cargo run --release -- some.pdf
```

Cross-platform: macOS, Windows, and Linux (egui/winit). Prebuilt binaries for
all three are attached to [GitHub releases](https://github.com/andredegrenier/evo/releases);
CI builds and tests every platform. Keyboard shortcuts use ⌘ on macOS and
Ctrl elsewhere.

## Known limitations

- **Preview fidelity**: hayro is a young pure-Rust PDF renderer. Some
  documents (exotic blend modes, unusual fonts) may not display pixel-perfect
  — a status-bar warning appears when that happens. This affects the on-screen
  preview only: exports reuse the original PDF bytes, so output fidelity is
  never reduced.
- Encrypted / password-protected PDFs are not supported.
- Text boxes export using the built-in Helvetica (standard-14) font; on screen
  they render with the bundled, metrically-compatible Liberation Sans.
- OCR models (~10 MB, by [Robert Knight](https://github.com/robertknight/ocrs-models),
  CC-BY-SA-4.0) are downloaded on first use into the library's `models/`
  folder — they are not bundled with the binary. Offline machines can place
  `text-detection.rten` and `text-recognition.rten` there manually.
- Saving always re-serializes through lopdf (use *Save As*; evo never
  overwrites your original in place).
- evo ships no model weights. The built-in model (Qwen3-4B-Instruct-2507,
  © Alibaba Cloud, Apache-2.0, quantized GGUF from community repos) downloads
  on first use into the library's `models/llm/` folder and is sha256-verified.
  A generated document uses the same standard-14 Helvetica as text boxes, so
  characters outside its encoding are written as `?` (the log says when).
- The MCP server binds 127.0.0.1 only and is off by default; treat the bearer
  token like a password.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Bundled [Liberation Sans](https://github.com/liberationfonts/liberation-fonts)
font is under the SIL Open Font License
([assets/fonts/LICENSE-LiberationSans](assets/fonts/LICENSE-LiberationSans)).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
