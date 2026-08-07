# evo

A simple PDF editor written entirely in Rust — Preview-like viewing with
Bluebeam-style precision markup.

evo aims for the feel of macOS Preview (open, scroll, zoom, mark up, print)
combined with the thing Preview never gives you: **exact control over your
markups** — numeric X/Y/W/H editing, snapping with alignment guides, and
one-click centering on the page.

**Pure Rust.** No PDFium, no MuPDF, no native PDF library at all. Rendering is
[hayro](https://github.com/LaurenzV/hayro), editing/writing is
[lopdf](https://github.com/J-F-Liu/lopdf), and the UI is
[egui](https://github.com/emilk/egui).

## Features

**Viewing**
- Continuous vertical page layout with smooth pan and zoom (pinch, ⌘+/⌘−,
  fit-width, actual size)
- Background rendering with progressive quality — stays responsive on large
  documents
- Page thumbnails sidebar

**Markup** — <kbd>V</kbd> select, <kbd>H</kbd> highlight, <kbd>T</kbd> text,
<kbd>R</kbd> rectangle, <kbd>O</kbd> ellipse, <kbd>L</kbd> line,
<kbd>A</kbd> arrow, <kbd>P</kbd> pen
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
- Insert pages from other PDFs into the open document, or combine several
  PDFs into one (File menu, or drop multiple files onto the window)

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
