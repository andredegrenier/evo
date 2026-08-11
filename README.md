# evo

A PDF editor in Rust — Preview-like viewing with Bluebeam-style precision
markup, a searchable document library, and a local AI assistant.

evo aims for the feel of macOS Preview (open, scroll, zoom, mark up, print)
combined with the thing Preview never gives you: **exact control over your
markups** — numeric X/Y/W/H editing, snapping with alignment guides, and
one-click centering on the page.

**Pure-Rust PDF pipeline, with one seam.** Parsing, editing and writing are
Rust and only Rust — [lopdf](https://github.com/J-F-Liu/lopdf) writes,
[hayro](https://github.com/LaurenzV/hayro) parses and extracts text, the UI is
[egui](https://github.com/emilk/egui). *Drawing* the page is the single job
with two engines: **PDFium** (Chrome's rasterizer, BSD-3-Clause, bound
dynamically at runtime and shipped in the release artifacts) draws by default,
and hayro draws when it is absent — see [Rendering engine](#rendering-engine).
(Scripting embeds Lua 5.4 and the optional built-in model runs on
[llama.cpp](https://github.com/ggml-org/llama.cpp); with PDFium those are the
only non-Rust components, and evo still builds and runs with a pure-Rust
`--no-default-features` toolchain.)

## Features

**Viewing**
- Two rasterizers behind one seam: **PDFium** (Chrome's — the default, and it
  ships inside every release download) or the pure-Rust **hayro**, chosen in
  Preferences
- Continuous vertical page layout with smooth pan and zoom (pinch, ⌘+/⌘−,
  fit-width, actual size)
- Background rendering with progressive quality — stays responsive on large
  documents
- Page thumbnails sidebar
- **Password-protected PDFs**: a masked prompt on open, and the password is
  then used for everything the document does — rendering, find, chat, export.
  It is held for the session only and never written down. Adding one to the
  library asks first, then decrypts it once (see
  [Known limitations](#known-limitations))

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
<kbd>A</kbd> arrow, <kbd>P</kbd> pen, <kbd>C</kbd> cloud, <kbd>Y</kbd> polygon,
<kbd>⇧Y</kbd> polyline, <kbd>S</kbd> stamp, <kbd>⇧S</kbd> image stamp,
<kbd>N</kbd> sequence, <kbd>G</kbd> pan (or hold space)
- Every markup has 8 resize handles (shift = lock aspect), drag to move,
  arrow keys to nudge (shift = 10 pt)
- **Polygons, polylines and revision clouds**: click each corner, then double
  click or press <kbd>Enter</kbd> to finish (<kbd>Esc</kbd> abandons); the
  Cloud tool drags one out around a rectangle. Corners are dragged
  individually, and the Inspector turns the scallops on, off, and up. They
  export as real `/Polygon` and `/PolyLine` annotations, clouds included
- **Stamps**: the six standard ones (Approved, Not Approved, Draft, Final,
  Confidential, For Comment) or any words you like, from a gallery popover.
  `%date`, `%user` and `%filename` are filled in as the stamp is placed — the
  text is fixed from then on, so a drawing means what it meant on the day
- **Image stamps** (<kbd>⇧S</kbd>): pick a PNG (up to 2 MB — a logo, a scanned
  signature) and drop it on the page; transparency survives the export as a
  soft mask
- **Sequences** (<kbd>N</kbd>): click to drop numbered callouts, 1, 2, 3…, with
  an optional prefix. Picking the tool up again reads the page and carries on
  from the highest number already on it
- **Several at once**: shift-click or drag a marquee with the Select tool to
  take a set, then move, nudge, restyle or delete them together — one undo step
  for the lot. <kbd>⌘G</kbd> makes a selection into a group that is picked up
  as a unit afterwards; <kbd>⇧⌘G</kbd> breaks it up again
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

## Phone access (`evo serve`)

`evo serve` is the same binary with no window: it puts your library on a phone.
Point Safari at it, add it to the home screen, and evo is an app — library and
search, a page viewer that swipes and pinches, touch markup, document chat, and
an agent chat that can **drive evo** ("find the page about the roof detail and
highlight it" opens the document and draws the highlight).

```sh
evo serve init                 # choose a password; prints an otpauth:// URI
evo serve                      # listens on 0.0.0.0:8443
```

`serve init` prints a QR-scannable URI for your authenticator app. Then
`evo serve` starts the server; `--bind`, `--port`, `--data-dir`, `--config`,
`--insecure-http` and `--trust-proxy` are the flags. `evo fetch-model` downloads
model weights on a machine with no Preferences pane.

**It is a progressive web app**, hand-written with no JavaScript dependencies:
an installable app shell, a service worker that caches the shell and never an
API answer, and pages served as immutable PNGs with the markup as a separate
SVG overlay. iOS installs it to the home screen only over HTTPS, which is what
the deployment guide is mostly about.

**Security model.** A password (argon2id) *and* a TOTP code from an
authenticator app, required together, on every route that is not the login page
or the static shell. Sessions are 32-byte tokens stored only as hashes, in a
`__Host-` cookie that is `Secure`, `HttpOnly` and `SameSite=Strict`; mutations
also require a custom header, so a cross-site form cannot make one. Codes are
single-use, logins are rate-limited per client address, and every secret
comparison is constant-time. Everything is `0600` on disk and nothing is in the
repository.

The intended deployment is **loopback behind a tunnel**: the server binds
`127.0.0.1` and Tailscale (or a Cloudflare tunnel, or Caddy) provides the
certificate and the route. That opens no ports, and the password and code are
still required inside the tunnel.

The library `evo serve` opens is its own — one process may hold a library at a
time, so the desktop app and the server cannot share a directory concurrently.
Documents get to the server by upload; the formats are identical either way.

Deploying it to a Debian machine, end to end — building, installing, the
systemd unit, the three ways to reach it from a phone, choosing a model for the
hardware you have, backups: **[deploy/debian/RUNBOOK.md](deploy/debian/RUNBOOK.md)**.

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

## Rendering engine

evo parses, edits and writes PDFs in pure Rust and always will. **Drawing** the
page is the one job where being the only implementation of a twenty-year-old
specification is a liability, so it is the one job with two engines:

| | |
|---|---|
| **PDFium** | The rasterizer inside Chrome and Edge, and the one most PDF producers actually test against. Used by default. BSD-3-Clause; bound dynamically at runtime, so evo still builds with no C toolchain. |
| **hayro** | evo's original pure-Rust renderer. Still what draws SVG exports and reads positioned text in every mode, and what draws pages when PDFium is not available. |

Preferences → **Rendering** switches between them (*Automatic* means PDFium
when its library is present, hayro when it is not), shows where the library was
found, and offers to download one. The status bar names whichever drew the
document you are looking at.

Release downloads ship the PDFium library inside the artifact — next to the
binary in the tarball or zip, and in `evo.app/Contents/Frameworks/` on macOS —
so there is nothing to do. A build from source can fetch it:

```sh
evo fetch-pdfium                              # into evo's data directory
cargo run -- fetch-pdfium --into target/debug # beside a development binary
```

The download is pinned by version **and** SHA-256 in
[`deploy/pdfium.lock`](deploy/pdfium.lock), which the release workflow and the
binary both read. `EVO_PDFIUM_PATH` (a file or the directory holding one)
overrides the search for anyone building PDFium themselves. `evo serve` chooses
its engine with `"engine": "Auto" | "Hayro" | "Pdfium"` in `serve/config.json`;
cached page images are named after the engine that drew them, so changing it
never serves stale pixels.

**How close are they?** [`docs/fidelity-report.md`](docs/fidelity-report.md) is
the measurement: a few hundred PDFs from the [veraPDF
corpus](https://github.com/veraPDF/veraPDF-corpus) drawn twice and compared,
page by page. It is generated by a harness that also hashes every hayro page,
so a change in what the pure-Rust renderer draws cannot pass unnoticed:

```sh
cargo run -p xtask -- fidelity                    # check against the baseline
cargo run -p xtask -- fidelity --corpus fixtures  # committed PDFs only, offline
cargo run -p xtask -- fidelity --bless            # re-record baseline + report
```

Corpora are downloaded and cached, never committed. The numbers live in
`xtask/fidelity-baseline.json`, one section per platform. As of v0.6.0: 362
documents, 371 pages, **median divergence 0.009** of 255, and 8 pages far
enough apart to be worth a look — each one listed in the report.

**How fast are they?** The timing harness builds a thousand-page document with
real text and vector art and puts it through the paths a person waits on —
opening, jumping to a page, ⌘F, `evo serve`, a scroll from end to end:

```sh
cargo run -p xtask -- perf            # every measurement
cargo run -p xtask -- perf worker     # just the render worker
```

Release mode only, one test at a time, and the document is generated rather
than committed. Set `EVO_PDFIUM_PATH` to measure both engines. On a 1,000-page
document, on an M-series Mac, in release:

| | target | measured |
|---|---|---|
| Open the document | < 2 s | **1.1 ms** |
| Draw a page after a jump (worst of four) | < 500 ms | **3.0 ms** PDFium · 11 ms hayro |
| Draw the page you are on after a 24-page scroll burst | < 500 ms | **3.0 ms** PDFium · 11 ms hayro |
| ⌘F, first page of matches | < 1 s | **7.6 ms** |
| `evo serve`, render page 500 | < 1.5 s | **63 ms** PDFium · 19 ms hayro |
| Peak texture memory over an end-to-end scroll | ≤ 384 MB | **382 MB** |

**Does it hold up under abuse?** Thirteen [proptest](https://proptest-rs.github.io/proptest/)
properties run on every push — bytes that are not a PDF, the sample truncated
at every offset, bit-flipped and spliced so every xref offset moves, arbitrary
JSON at the markup API — each asserting evo answers with an error rather than a
panic. They found two bugs that lost work (a markup coordinate JSON can hold
and an `f32` cannot, bricking a document's sidecar; a hayro panic on a damaged
encryption dictionary taking the whole app with it), both fixed and both kept
as regressions among the twelve deliberately-broken files in
`tests/fixtures/broken/`. A [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
rig (`fuzz/`, five targets, weekly on nightly, never blocking) searches the
same doors for longer: 7.7 million executions, no crash.

## Known limitations

- **Preview fidelity**: with the pure-Rust hayro renderer, some documents
  (exotic blend modes, unusual fonts) may not display pixel-perfect — a
  status-bar warning appears when that happens. PDFium, which release downloads
  already carry and which draws by default, is the answer; either way this
  affects the on-screen preview only, since exports reuse the original PDF
  bytes.
- **Password-protected PDFs open, but copies of them do not stay protected.**
  Export, print and Save As write a decrypted PDF — evo says so before it
  writes one. Adding one to the library decrypts it once, with your consent, so
  that indexing, OCR and `evo serve` need no password afterwards; the original
  file on disk is never touched. Re-encrypting on save is still to come. Uploading
  an encrypted PDF to `evo serve` is refused (422) — add it from the desktop.
- **Markup sidecars are version 2 as of v0.6.** evo 0.6 reads v1 files, but
  evo 0.5 cannot read a v2 file containing polygons, polylines, clouds, stamps,
  image stamps or groups. Only the sidecars of library documents are affected;
  exported PDFs are readable by anything.
- **The phone creates highlights and text boxes only.** Every other kind —
  clouds, polygons, stamps, sequences — is drawn, moved and deleted on the
  phone, but made on the desktop.
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

Release artifacts also carry the PDFium shared library (BSD-3-Clause, The
PDFium Authors), built by
[pdfium-binaries](https://github.com/bblanchon/pdfium-binaries) (Apache-2.0)
and bound with [pdfium-render](https://github.com/ajrcarey/pdfium-render)
(MIT OR Apache-2.0). Full notices are in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md), which ships in every
artifact.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
