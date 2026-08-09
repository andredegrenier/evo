# Third-party notices

evo itself is MIT OR Apache-2.0 (see `LICENSE-MIT` and `LICENSE-APACHE`). This
file covers the components redistributed **inside evo's release artifacts** —
the ones you receive as files rather than as source dependencies. Every other
crate evo builds against is a permissively licensed Rust dependency; run
`cargo tree` on the source for that list.

## PDFium

`libpdfium.dylib` / `libpdfium.so` / `pdfium.dll` shipped beside the evo
binary (and inside `evo.app/Contents/Frameworks/` on macOS) is **PDFium**, the
PDF rendering library from the Chromium project.

- Copyright 2014 The PDFium Authors. All rights reserved.
- Licensed under the **BSD 3-Clause "New" or "Revised" License**.
- Upstream: <https://pdfium.googlesource.com/pdfium/>

```
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

    * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
    * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
    * Neither the name of Google Inc. nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

PDFium bundles further third-party components of its own (FreeType, libjpeg-turbo,
libpng, zlib, ICU, Little CMS, OpenJPEG, libtiff, Abseil, AGG and others), each
under its own permissive licence. Their full texts ship inside the
`pdfium-binaries` archive under `licenses/`, and are reproduced upstream at
<https://pdfium.googlesource.com/pdfium/+/refs/heads/main/LICENSE>.

## pdfium-binaries

The prebuilt PDFium archives evo downloads and bundles are built by
**bblanchon/pdfium-binaries**, whose packaging is licensed **Apache-2.0**.

- <https://github.com/bblanchon/pdfium-binaries>
- The exact release evo pins is recorded in `deploy/pdfium.lock`, together with
  the SHA-256 of every archive it will accept.

## pdfium-render

evo's Rust bindings to PDFium are **pdfium-render** by Alastair Carey, licensed
**MIT OR Apache-2.0**.

- <https://github.com/ajrcarey/pdfium-render>

## Fonts and models

Fonts embedded in the binary and the OCR / language models evo can download are
covered where they are used: the OCR models (CC-BY-SA-4.0, Robert Knight) and
the language-model weights are downloaded on request and are never part of a
release artifact.
