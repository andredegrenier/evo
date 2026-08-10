# fuzz

Five libFuzzer targets over the code that reads files evo did not write.

The properties in `src/robustness.rs` run on every push and are sized for
seconds. These are the same rules with hours behind them and a coverage-guided
mutator choosing the inputs. Between them they cover both doors: the properties
catch a regression the day it lands, and this catches the case nobody thought
of.

## The targets

| Target | What it runs | Why it is a door |
|---|---|---|
| `fuzz_load_bytes` | `Document::load_bytes`, with and without a password | Every file evo opens comes through it. (The password call is there for the day the corpus can hold an encrypted document again — see below.) |
| `fuzz_lopdf_roundtrip` | `lopdf::Document::load_mem` → `save_to` → load → save | evo reads with hayro and writes with lopdf, so every export re-reads untrusted bytes through a second, unrelated parser. |
| `fuzz_export` | `export_pdf_bytes` over a fuzzed document, empty markup and a shape of every kind, annotated and flattened | A saved copy that will not open again is worse than a refusal: the person finds out when they need the file. |
| `fuzz_extract` | `extract_all_pages` | The deepest walk there is — every content stream interpreted, every font parsed — and it runs on a background thread where a panic shows nobody anything. |
| `fuzz_markup_json` | markup read from JSON, its version tag, the round trip back to JSON, and export onto a real page | The one part of evo's state that arrives as a document from outside. |

Each asserts more than "did not crash". The interesting properties are in the
target files, at the top.

## Running it

cargo-fuzz needs nightly. It does not need to be your default toolchain.

```sh
rustup toolchain install nightly          # once
cargo +nightly install cargo-fuzz         # once

./fuzz/seed-corpus.sh                     # start from evo's own fixtures
cargo +nightly fuzz run fuzz_load_bytes -- -max_total_time=900
cargo +nightly fuzz list                  # the five names
```

`-rss_limit_mb=4096 -max_len=262144` are worth adding for the document targets:
the default 2 GB limit is reached by an ordinary large PDF, and an input longer
than a quarter of a megabyte is not finding anything a shorter one would not.

The first build is a long one. `fuzz/` depends on evo as a library, which is
what `src/lib.rs` exists for, and building it with a sanitizer compiles egui,
tantivy, ocrs and a vendored Lua. `default-features = false` in `fuzz/Cargo.toml`
at least drops llama.cpp and PDFium — neither of which any target calls, and one
of which is a C++ build. It is a one-off; the fuzzing itself is fast.

### One known crash, and what it costs

hayro-syntax 0.7.2 panics on a damaged encryption dictionary. evo catches it —
every parse goes through `doc::open_pdf`, which answers "not a valid PDF file" —
so the *application* is fine, and `src/robustness.rs` asserts exactly that.

libfuzzer-sys is not fine with it. It installs a panic hook that aborts the
process before unwinding, on purpose, so that a `catch_unwind` somewhere in the
fuzzed code cannot hide a bug from the fuzzer. That is the right default and it
means these targets cannot see evo's guard: a caught panic is still a dead run.

So `seed-corpus.sh` seeds no encrypted document, or every run would end in its
first second having rediscovered the same upstream bug. The cost is that the
decryption path is not fuzzed; it is covered by a property instead. Put the
seeds back when hayro clamps the length — the comment in `seed-corpus.sh` says
where.

If a run does abort with `range end index 32 out of range for slice of length
16` at `hayro-syntax-*/src/crypto/mod.rs`, that is this one, and not a new
finding.

### When one crashes

```sh
cargo +nightly fuzz tmin fuzz_load_bytes fuzz/artifacts/fuzz_load_bytes/crash-<hash>
```

Then commit the minimized file to `tests/fixtures/broken/` under a name that
says what is wrong with it, add it to `BROKEN` in `src/robustness.rs`, and fix
the bug. A crasher that only lives in `fuzz/artifacts/` is one that comes back.

`tests/fixtures/broken/encrypt-length-overruns-md5.pdf` is what that looks like:
one flipped bit in the AES-256 fixture, `/R 6` reading `/R 4`, which sends a
256-bit `/Length` down the revision-4 key derivation and asks hayro-syntax 0.7.2
for the first 32 bytes of a 16-byte MD5 digest. Found by a property rather than
by this, fixed at `crate::doc::open_pdf`, and kept forever as a named test.

## Why it is not a workspace member

`fuzz/Cargo.toml` carries its own empty `[workspace]` table, which makes it a
workspace root rather than one of evo's members. A member would put nightly and
a sanitizer build in front of `cargo build`, `cargo test` and every clippy line
in CI, for code nobody runs on a push. The cost is that `cargo fmt --all` and
`cargo clippy --all-targets` do not reach it either:

```sh
cargo fmt --manifest-path fuzz/Cargo.toml
cargo +nightly clippy --manifest-path fuzz/Cargo.toml --all-targets
```

## In CI

`.github/workflows/fuzz.yml` runs every target for fifteen minutes, weekly and
on demand. It never runs on a push and it never blocks a merge: fuzzing is a
search, and a search that fails to finish is not a broken build. A crash uploads
the input as an artifact.

Corpora are not committed. `seed-corpus.sh` rebuilds a starting one from
`tests/fixtures/` in a second, and the corpus a long run grows is a cache, not a
source file.

## Not done

Applying to [OSS-Fuzz](https://google.github.io/oss-fuzz/) — continuous fuzzing
on Google's machines, with the corpus kept between runs and crashes reported
privately. It wants a project that is a) open source and b) widely enough used
to be worth their CPU, and the build integration is a Dockerfile plus a
`build.sh` that is mostly what `fuzz/` already is. Worth doing once evo has
users; noted here so it is not forgotten.
