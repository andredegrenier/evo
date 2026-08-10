#!/usr/bin/env bash
# Fill each target's corpus with the documents evo already has.
#
# A fuzzer starting from nothing spends its first hours discovering that a PDF
# begins with "%PDF". Starting it from a real file means the first minute is
# already mutating structure that parses.
#
# Safe to re-run: files are copied under their content hash, so a corpus that
# has grown from a previous session keeps everything it learned.
#
# WHAT IS NOT SEEDED: anything encrypted.
#
# hayro-syntax 0.7.2 panics on a damaged encryption dictionary (see
# `doc::open_pdf`). evo catches that panic and answers "not a valid PDF file",
# so the application is fine -- but libfuzzer-sys installs a panic hook that
# aborts the process before unwinding, deliberately, so that a `catch_unwind`
# in the fuzzed code cannot hide a bug from the fuzzer. A caught panic is
# therefore still a dead run. Seed one encrypted document and every session
# rediscovers the same upstream bug within minutes and looks at nothing else.
#
# Put them back when hayro clamps that length. Until then the decryption path
# is covered by `a_damaged_protected_document_is_refused_rather_than_fatal` in
# src/robustness.rs, which asserts evo's actual contract and passes.
#
# Everything else is fair game, including the hand-made broken files: a
# truncated xref and a page tree that contains itself are exactly the shapes a
# mutator takes a long time to invent.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fixtures="$here/../tests/fixtures"

targets=(fuzz_load_bytes fuzz_lopdf_roundtrip fuzz_export fuzz_extract)

copied=0
for target in "${targets[@]}"; do
  mkdir -p "$here/corpus/$target"
  for pdf in "$fixtures"/*.pdf "$fixtures"/broken/*.pdf; do
    [ -f "$pdf" ] || continue
    case "$(basename "$pdf")" in encrypt*) continue ;; esac
    hash="$(shasum -a 256 "$pdf" | cut -c1-16)"
    cp -n "$pdf" "$here/corpus/$target/$hash" 2>/dev/null || true
    copied=$((copied + 1))
  done
done

# fuzz_export takes its first byte as the "flatten" flag and the rest as the
# document, so a seed for it is a PDF with one byte in front.
for pdf in "$here"/corpus/fuzz_export/*; do
  [ -f "$pdf" ] || continue
  case "$(head -c 4 "$pdf")" in
    '%PDF') printf '\0' | cat - "$pdf" > "$pdf.tmp" && mv "$pdf.tmp" "$pdf" ;;
  esac
done

# The markup target eats JSON, not PDFs.
mkdir -p "$here/corpus/fuzz_markup_json"
cat > "$here/corpus/fuzz_markup_json/seed-highlight.json" <<'JSON'
{"version":2,"annotations":[{"id":1,"page":0,"kind":"Highlight","rect":{"min":{"x":72.0,"y":100.0},"max":{"x":172.0,"y":120.0}},"style":{"stroke":{"r":0,"g":0,"b":0,"a":0},"stroke_width":0.0,"fill":{"r":255,"g":235,"b":59,"a":255},"opacity":0.35}}]}
JSON
cat > "$here/corpus/fuzz_markup_json/seed-shapes.json" <<'JSON'
{"version":2,"pages":{"states":[{"rotation":"None","deleted":false},{"rotation":"None","deleted":false}],"order":[0,1],"source_of":[0,1]},"annotations":[{"id":1,"page":0,"kind":{"Polygon":{"points":[{"x":100.0,"y":600.0},{"x":300.0,"y":600.0},{"x":300.0,"y":700.0}],"cloudy":1.5}},"rect":{"min":{"x":100.0,"y":600.0},"max":{"x":300.0,"y":700.0}},"style":{"stroke":{"r":220,"g":38,"b":38,"a":255},"stroke_width":2.0,"fill":{"r":0,"g":0,"b":0,"a":0},"opacity":1.0},"group":3},{"id":2,"page":1,"kind":{"PolyLine":{"points":[{"x":72.0,"y":200.0},{"x":320.0,"y":200.0}],"arrow_end":true}},"rect":{"min":{"x":72.0,"y":200.0},"max":{"x":320.0,"y":200.0}},"style":{"stroke":{"r":220,"g":38,"b":38,"a":255},"stroke_width":2.0,"fill":{"r":0,"g":0,"b":0,"a":0},"opacity":1.0}},{"id":3,"page":0,"kind":{"Stamp":{"text":"APPROVED","font_size":20.0}},"rect":{"min":{"x":100.0,"y":700.0},"max":{"x":260.0,"y":744.0}},"style":{"stroke":{"r":193,"g":39,"b":45,"a":255},"stroke_width":1.5,"fill":{"r":0,"g":0,"b":0,"a":0},"opacity":1.0}},{"id":4,"page":0,"kind":{"TextBox":{"text":"check this","font_size":11.0,"align":"Left"}},"rect":{"min":{"x":72.0,"y":500.0},"max":{"x":240.0,"y":540.0}},"style":{"stroke":{"r":30,"g":30,"b":46,"a":255},"stroke_width":0.0,"fill":{"r":255,"g":245,"b":180,"a":255},"opacity":0.95}}]}
JSON

echo "seeded $copied document(s) across ${#targets[@]} targets, plus markup JSON"
