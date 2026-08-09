# Running `evo serve` on a Debian box

The operator's guide: how to get `evo serve` onto a headless Debian machine,
how to reach it from an iPhone, and what to do when something is wrong.

Written for the target this was designed against — an Intel x86_64 box you own,
running Debian 12 or 13, reachable over SSH. There is an AWS variant in
[aws-appendix.md](aws-appendix.md); it is an outline, not a recommendation, and
nothing in it has been run.

**Shape of the thing.** `evo serve` binds `127.0.0.1:8443` and nothing else. It
is a different program wearing the same binary as the desktop app: no window,
its own JSON configuration, a password and a TOTP code in front of every route
that matters. What makes it reachable from a phone is a tunnel or a reverse
proxy in front of it — pick one of the three tiers below. Until you do, the
server is only reachable from the box itself, which is the right order to do
this in.

**One library, one process.** redb allows a single process per database file.
The desktop app and `evo serve` cannot hold the same library at once; if you
try, the server refuses with a sentence saying so rather than a lock error.
The server's library is its own — documents get there by being uploaded from
the phone, not by syncing from your Mac. The *formats* are shared, so a
highlight made on the phone opens in the desktop app if you ever point it at
the same directory.

---

## Contents

1. [Getting a binary onto the box](#1-getting-a-binary-onto-the-box)
2. [Installing](#2-installing)
3. [First run: password and authenticator](#3-first-run-password-and-authenticator)
4. [Getting to it from the phone](#4-getting-to-it-from-the-phone)
5. [Choosing a model](#5-choosing-a-model)
6. [Installing the PWA on the iPhone](#6-installing-the-pwa-on-the-iphone)
7. [Updating](#7-updating)
8. [Backups](#8-backups)
9. [When something is wrong](#9-when-something-is-wrong)

---

## 1. Getting a binary onto the box

Two paths. **Path A is the recommended one.**

Do not use the Linux binary from GitHub Releases for this. It is built on
Ubuntu 24.04 (glibc 2.39) and will not start on Debian 12 (glibc 2.36) — the
symptom is `version 'GLIBC_2.39' not found`. It is fine on Debian 13.

### Path A — build natively on the box (recommended)

The target is Intel x86_64 and so is the box: no cross-compilation, no
emulation, and the resulting binary is linked against exactly the libraries it
will run against.

```sh
# On the box, as your ordinary login user (not root, not evo).

# 1. Toolchain and headers. eframe and rfd link GTK even though `evo serve`
#    never opens a window, so the GUI headers are needed to build it.
sudo apt-get update
sudo apt-get install -y build-essential cmake clang pkg-config \
    libgtk-3-dev libxkbcommon-dev libwayland-dev curl git

# 2. Rust. Debian's rustc is usually too old; evo needs 1.92 (edition 2024).
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustc --version        # expect 1.92 or newer

# 3. Source.
git clone https://github.com/andredegrenier/evo
cd evo

# 4. Build. --features s3 compiles in the S3 blob backend; it stays off at
#    runtime unless serve/config.json asks for it, so this only buys you the
#    option of switching to a bucket later without another build.
cargo build --release --features s3
```

The first build compiles llama.cpp from source. **On an old Intel chip expect
20–60 minutes**, most of it in the C++ compile, and expect a couple of
gigabytes in `target/`. Later builds are minutes.

If the machine is short on RAM the linker is what falls over first. `cargo
build -j2 --release --features s3` uses fewer parallel jobs.

If llama-cpp-2 will not build at all — this is the one dependency that has
never been exercised on this box — you still have a working evo without a local
model:

```sh
cargo build --release --no-default-features --features s3
```

That drops the built-in model. Chat then requires an external endpoint
(§5, step 3), which on an old box is likely what you wanted anyway.

### Path B — build in Docker on the Mac (fallback)

For when the box cannot host a toolchain, or you want the binary before the box
is reachable.

```sh
# On the Mac, in the repository.
./deploy/debian/build-in-docker.sh
# -> target/docker-amd64/release/evo
```

The container is `rust:1.92-bookworm` with the same apt list, built
`--platform linux/amd64`. On Apple Silicon that runs under emulation: it works,
and the llama.cpp C++ build is markedly slower than it would be natively —
budget the better part of an hour. If the box has any spare time at all, Path A
wins.

**glibc direction matters.** A binary runs on the glibc it was built against
*or newer*, never older. bookworm (2.36) is chosen so the output runs on Debian
12 and 13 both. Building in a `trixie` container would produce something Debian
12 refuses to start.

Then:

```sh
scp target/docker-amd64/release/evo you@box:/tmp/evo
```

---

## 2. Installing

Copy `deploy/debian/` to the box (or clone the repository there — Path A
already did) and run:

```sh
sudo ./deploy/debian/install.sh --binary /tmp/evo
# or, with a source tree already on the box:
sudo ./deploy/debian/install.sh
```

It is re-runnable: every step checks before it changes anything, so running it
again after a rebuild replaces the binary and leaves the rest alone.

What it does:

| Step | What |
|---|---|
| Preflight | Debian-ish? systemd? architecture? |
| Hardware | AVX2 / AVX-512, RAM, free disk — and a recommendation |
| Packages | `libgtk-3-0(t64) libxkbcommon0 libwayland-client0 ca-certificates`; `--build-deps` adds the Path A toolchain |
| Account | system user `evo`, home `/var/lib/evo`, no shell |
| Layout | `/var/lib/evo`, owned `evo:evo`, mode `0750` |
| Binary | `/usr/local/bin/evo`, after checking it actually starts here |
| Unit | `/etc/systemd/system/evo.service`, `daemon-reload` |
| PDFium | offers to download the pinned rendering library into `/var/lib/evo` (optional) |
| Smoke | times a real generation, if a model is present (see below) |
| Credentials | offers to run `evo serve init` |

It does **not** enable or start the service. The service cannot start before
`evo serve init` has written credentials, so starting it for you would only
produce a failed unit.

### The rendering library

A binary built on the box does not come with PDFium, so `evo serve` draws page
images with the pure-Rust hayro renderer until one is installed. The installer
offers to fetch it; you can also do it by hand at any time:

```sh
sudo -u evo env HOME=/var/lib/evo /usr/local/bin/evo fetch-pdfium
sudo systemctl restart evo
```

That downloads the exact PDFium release pinned in `deploy/pdfium.lock` and
checks its SHA-256 before unpacking it into
`/var/lib/evo/.local/share/evo/pdfium/<version>/`. It is optional: everything
works without it, only with hayro's fidelity rather than Chrome's. Binaries
downloaded from GitHub releases already carry the library beside them and need
none of this.

### The hardware smoke test

Estimates about inference speed are worthless; a measurement is not. So when a
model file is present, the installer times a real generation rather than
guessing from the CPU flags.

There is a wrinkle worth stating plainly: **the `evo` binary has no one-shot
`generate` subcommand to time.** Its entry points are `serve`, `mcp-serve` and
`fetch-model`, and every generation path sits behind either the GUI or an
authenticated HTTP route. What does exist is an `#[ignore]`d test in the source
tree that loads a downloaded model and streams a real completion. On a Path A
box the source tree is right there, so that test *is* the measurement, and the
installer runs it. Without a source tree the installer prints the command and
says why it could not run it:

```sh
XDG_DATA_HOME=/var/lib/evo/.local/share \
EVO_LLM_TEST_MODEL=qwen3-4b-instruct-2507 \
  cargo test --release --bin evo -- --ignored --exact --nocapture \
  llm::backend::tests::the_builtin_backend_answers_and_streams
```

Read the harness's own duration line, not the wall time (which includes
compiling the test binary). What it measures is a **cold** run: model load plus
a short real completion. Every answer after the first skips the load.

| Cold figure | Reading |
|---|---|
| under 20s | comfortable — keep the built-in model |
| 20–60s | usable if you are patient; try `qwen3-1.7b` |
| over 60s | point the configuration at an external endpoint |

The other honest measurement is the real one: start the service, ask a question
from the phone, and watch `journalctl -fu evo`.

---

## 3. First run: password and authenticator

This is done over SSH. Nothing needs a screen on the box, and nothing needs the
server to be reachable yet.

```sh
sudo -u evo env HOME=/var/lib/evo \
  /usr/local/bin/evo serve init --data-dir /var/lib/evo
```

It asks for a password and then prints:

```
evo serve is set up. Credentials are in /var/lib/evo/serve/auth.json.

Add this to an authenticator app -- as a QR code, or by hand:

  otpauth://totp/evo:you?secret=...&issuer=evo&...

  secret: JBSWY3DPEHPK3PXP...
```

**Run it as the `evo` user**, as above: files it writes have to belong to the
account the service runs as. `auth.json` is written `0600` and holds an
argon2id hash of the password plus the TOTP secret. It never leaves the box.

**The password is echoed as you type** — evo has no terminal crate and says so
rather than surprising you. If that matters on your terminal:

```sh
sudo -u evo env HOME=/var/lib/evo EVO_SERVE_PASSWORD='...' \
  /usr/local/bin/evo serve init --data-dir /var/lib/evo
history -d $((HISTCMD-1))     # and clear it from the shell history
```

### Enrolling the authenticator

Two ways, both over the SSH session you already have:

1. **By hand** — type the `secret:` line into 1Password / Authy / Google
   Authenticator as a time-based secret.
2. **By QR** — paste the whole `otpauth://` URI into a QR generator you trust,
   or let evo draw it: sign in from the phone with the password *alone* before
   any code has been accepted, and the server answers `{"enroll": true}` and
   offers the QR code at `/api/setup-qr`. The web app shows it for you.

The first code the server accepts finishes enrolment. Until then, a password
alone only ever gets you the QR code — never the library.

After that, every sign-in needs the password **and** a current code. A code is
accepted once and only once (RFC 6238 §5.2): one copied off a screen is
worthless a minute later, and replaying one gets "that code has already been
used".

### Starting it

```sh
sudo systemctl enable --now evo
journalctl -fu evo
```

The log's first lines are the address, the document count, and which model the
configuration points at — the two things that are silently wrong most often.

```sh
curl -s http://127.0.0.1:8443/api/health
```

---

## 4. Getting to it from the phone

Three tiers. **Tailscale (a) is the recommendation** and it is not close: a
valid certificate, no open ports, nothing to renew, no DNS to own.

TOTP is enforced in every tier. None of these is a security boundary evo relies
on — they are how the traffic gets there, not what keeps strangers out. The
password and the code do that, all three ways.

A certificate is not optional in practice: iOS will not install a PWA to the
home screen, and will not register a service worker, over plain HTTP to a LAN
address. Plain `http://192.168.1.x:8443` works in Safari as a page; it will not
become an app icon.

| Tier | Mechanism | HTTPS / installable | Ports you open |
|---|---|---|---|
| **(a) Tailscale** | `tailscale serve` in front of loopback | yes, valid cert | none |
| (b) Cloudflare Tunnel | `cloudflared` outbound to Cloudflare | yes, valid cert | none |
| (c) Direct | router forwards 443 → Caddy | yes, Let's Encrypt | 443 |

### (a) Tailscale — recommended

```sh
# On the box.
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up

# In the admin console (login.tailscale.com), once per tailnet:
#   DNS  -> MagicDNS               : enabled
#   DNS  -> HTTPS Certificates     : enabled
# Without those two, the ts.net name and its certificate do not exist.

# Put the loopback service on the tailnet, with TLS.
sudo tailscale serve --bg https / http://127.0.0.1:8443

# Confirm.
tailscale serve status
tailscale status          # the box's name is the first column
```

That gives you `https://<box>.<tailnet>.ts.net` with a certificate Tailscale
obtains and renews. Install Tailscale on the iPhone from the App Store, sign in
to the same tailnet, and open that URL.

Notes:

- Older Tailscale builds want `tailscale serve https / http://127.0.0.1:8443`
  without `--bg` and stay in the foreground; `--bg` (1.60+) persists it.
- `tailscale serve reset` undoes it.
- Do **not** use `tailscale funnel` unless you specifically want the service on
  the public internet. `serve` is tailnet-only, which is the point.
- No `--trust-proxy` concern here: the unit already sets it, and Tailscale sets
  `X-Forwarded-For` to the tailnet peer.

### (b) Cloudflare Tunnel

Needs a domain whose DNS Cloudflare manages.

```sh
# On the box.
curl -fsSL https://pkg.cloudflare.com/cloudflared-stable-linux-amd64.deb -o /tmp/cloudflared.deb
sudo dpkg -i /tmp/cloudflared.deb

cloudflared tunnel login                 # opens a URL to authorise
cloudflared tunnel create evo            # writes ~/.cloudflared/<uuid>.json
cloudflared tunnel route dns evo evo.example.com
```

`/etc/cloudflared/config.yml`:

```yaml
tunnel: evo
credentials-file: /root/.cloudflared/<uuid>.json
ingress:
  - hostname: evo.example.com
    service: http://127.0.0.1:8443
  - service: http_status:404
```

```sh
sudo cloudflared service install
sudo systemctl status cloudflared
```

Cloudflare terminates TLS at their edge; the tunnel is an outbound connection,
so no port is opened. Anyone who finds the hostname reaches the login page —
which is exactly as far as they get without the password and a code. Cloudflare
Access in front of it is an option if you want a second door.

### (c) Direct, with Caddy

Needs a domain you control and an address that does not move (or DDNS), plus a
router that will forward ports.

```sh
sudo apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt-get update && sudo apt-get install -y caddy

sudo cp deploy/debian/Caddyfile /etc/caddy/Caddyfile
sudo $EDITOR /etc/caddy/Caddyfile        # the hostname and the tls email
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

Forward **both** 80 and 443 to the box: 80 is how the ACME HTTP challenge is
answered, 443 is the service. Point `evo.example.com` at your address first —
Let's Encrypt validates before it issues, and its failure rate limits are
per-hostname and unkind.

This is the tier with an open port and a public hostname. Everything reaching
it hits the login route, which is rate-limited to 5 attempts per 5 minutes per
client address; the unit's `--trust-proxy` is what lets that counter see the
real client through Caddy.

---

## 5. Choosing a model

Where the weights live is worth knowing before anything else: **`--data-dir`
does not move them.** They go under the platform data directory of whichever
account runs evo — for the service account, that is:

```
/var/lib/evo/.local/share/evo/library/models/llm/
```

which is why the unit sets `HOME=/var/lib/evo` and why every `fetch-model`
command here runs as `evo`.

Work down this chain and stop at the first step that is comfortable.

### 1. The built-in 4B — if the hardware allows

Qwen3-4B-Instruct-2507, Q4_K_M, 2.5 GB on disk, roughly 4 GB resident. Wants
AVX2 (Haswell, 2013) at a minimum and about 6 GB of RAM to sit in without
fighting the page cache.

```sh
sudo -u evo env HOME=/var/lib/evo /usr/local/bin/evo fetch-model
```

`serve/config.json`:

```json
{
  "model": {
    "api": "Builtin",
    "base_url": "",
    "model": "",
    "builtin_model": "qwen3-4b-instruct-2507",
    "timeout_secs": 120
  }
}
```

### 2. The 1.7B — for an older or smaller box

1.1 GB, less than half the size, noticeably quicker.

```sh
sudo -u evo env HOME=/var/lib/evo /usr/local/bin/evo fetch-model qwen3-1.7b
```

```json
{ "model": { "api": "Builtin", "base_url": "", "model": "", "builtin_model": "qwen3-1.7b", "timeout_secs": 120 } }
```

**Caveat, and it is a real one:** Qwen3-1.7B is a hybrid-thinking model. It
reasons out loud before answering, so replies begin with its thinking rather
than with the answer, and short questions produce long preambles. The
catalogue entry says so too. If that reads as broken to you, prefer step 3.

### 3. An external endpoint — often the best answer

Zero code, no C++ toolchain, and on an old box a far better experience than
either of the above. The obvious pairing: Ollama on your Mac, reached over the
same tailnet as the phone.

```sh
# On the Mac.
ollama pull qwen3:8b
# Ollama binds loopback by default; let the tailnet reach it.
launchctl setenv OLLAMA_HOST 0.0.0.0:11434     # then restart Ollama
```

`serve/config.json` on the box:

```json
{
  "model": {
    "api": "Ollama",
    "base_url": "http://<mac>.<tailnet>.ts.net:11434",
    "model": "qwen3:8b",
    "builtin_model": "qwen3-4b-instruct-2507",
    "timeout_secs": 180
  }
}
```

`"api"` may also be `"OpenAiCompatible"` for llama.cpp's server, LM Studio,
vLLM and most other things — `base_url` then points at the host, and evo adds
`/v1/chat/completions` itself.

The Mac has to be awake and on the tailnet for chat to work. Everything else
in evo — library, search, viewer, markup — does not touch the model at all.

### Editing the configuration

```sh
sudo -u evo $EDITOR /var/lib/evo/serve/config.json
sudo systemctl restart evo
journalctl -u evo -n 20        # the startup line names the model in use
```

The whole file, with everything else at its defaults:

```json
{
  "model": { "api": "Builtin", "base_url": "", "model": "", "builtin_model": "qwen3-4b-instruct-2507", "timeout_secs": 120 },
  "assistant": { "enrich_enabled": false },
  "mcp_clients": [],
  "blobs": "local",
  "max_upload_mb": 200,
  "engine": "Auto"
}
```

`engine` picks the rasterizer that draws page images for the phone: `"Auto"`
(PDFium if its library is here, hayro otherwise), `"Hayro"`, or `"Pdfium"`.
Cached page PNGs are named after the engine that drew them, so changing this
re-renders rather than serving the other engine's pixels — the old files stay
on disk until you delete `/var/lib/evo/library/pagecache`. Like `mcp_clients`
it is configuration-file-only: it decides what every cached image on the server
looks like, which no HTTP request should be able to change.

A file that exists but does not parse is a startup error, on purpose: silently
ignoring a typo would point the server at the wrong model without saying so.

`mcp_clients` is configuration-file-only and deliberately not settable over the
API — an entry names a program to run, which is not something an HTTP route has
any business accepting.

---

## 6. Installing the PWA on the iPhone

Requires HTTPS, which means one of the three tiers in §4 is already working.

1. Open the URL in **Safari** (not Chrome — on iOS only Safari can install to
   the home screen).
2. Sign in: password, then the six-digit code. If the authenticator has not
   been enrolled yet, the password alone shows the QR code — scan it with the
   authenticator app, then sign in again with a code.
3. Share sheet → **Add to Home Screen** → Add.
4. Open it from the icon. Check:
   - [ ] no Safari chrome — it opens standalone, with the dark background
   - [ ] the library grid draws, thumbnails and all
   - [ ] a document opens; pages swipe horizontally and pinch-zoom crisply
     (zooming in re-requests a higher-resolution page)
   - [ ] the markup tool draws a highlight, and it survives a reload
   - [ ] chat streams a word at a time rather than arriving all at once
   - [ ] the agent tab can be asked to find and highlight something, and the
     viewer follows it
   - [ ] uploading a PDF from Files works and the document appears in the grid
   - [ ] airplane mode shows the shell and a sentence about being offline,
     rather than a Safari error page
5. Sessions last 30 days and slide forward on use, so this is not a daily
   ritual.

If "Add to Home Screen" is missing, the certificate is the reason nine times
out of ten — check the URL really is `https://` and not a LAN IP.

The app works without the service worker. If iOS evicts it (it does, on apps
you have not opened in a while), the first load is slower and everything still
functions.

---

## 7. Updating

```sh
# Path A, on the box:
cd ~/evo && git pull && cargo build --release --features s3
sudo ./deploy/debian/install.sh          # finds target/release/evo

# Path B, from the Mac:
./deploy/debian/build-in-docker.sh
scp target/docker-amd64/release/evo box:/tmp/evo
ssh box 'sudo /path/to/deploy/debian/install.sh --binary /tmp/evo'
```

Then:

```sh
sudo systemctl restart evo
journalctl -u evo -n 30
curl -s http://127.0.0.1:8443/api/status | head
```

`install.sh` replaces the file on disk; the running process keeps executing the
old one until it is restarted. The restart drops in-flight SSE streams — a chat
answer being typed on the phone stops mid-sentence. It is not a graceful
handover; do it when nobody is reading.

**The phone caches the app shell.** The service worker serves the cached HTML
and JS while fetching the new ones in the background, so the first open after
an update may still be the old build and the second will be the new one. To
force it: close the app from the app switcher, open it, close it, open it
again. Nothing under `/api/` is ever cached, so the library is never stale —
only the shell.

Neither the library nor credentials are touched by an update.

---

## 8. Backups

Everything that cannot be rebuilt is under **`/var/lib/evo`**:

| Path | What | Rebuildable? |
|---|---|---|
| `library/meta.redb` | document metadata, tags, summaries, chat transcripts, **and the markup** | no |
| `library/docs/` | the PDF bytes (local blob backend) | no |
| `library/index/` | the tantivy search index | yes, by re-import |
| `library/pagecache/`, `library/thumbs/` | rendered PNGs | yes, on demand |
| `library/models/` | OCR models (~10 MB) | yes, re-download |
| `.local/share/evo/library/models/llm/` | LLM weights (gigabytes) | yes, re-download |
| `serve/auth.json` | argon2id password hash + TOTP secret | no — losing it un-enrols the authenticator |
| `serve/sessions.json` | signed-in devices | yes (everyone signs in again) |
| `serve/config.json` | model, blobs, MCP servers | small; keep it anyway |

Markup never modifies the PDF — the original bytes in `docs/` are untouched and
your highlights live in the database, so `meta.redb` is the file whose loss
actually costs you work.

Stop the service first. redb and tantivy are memory-mapped: copying them while
they are open can capture a torn state.

```sh
sudo systemctl stop evo
sudo tar -czf /root/evo-$(date +%F).tar.gz -C /var/lib evo
sudo systemctl start evo
```

Restore is the reverse, onto a box where `install.sh` has already run:

```sh
sudo systemctl stop evo
sudo tar -xzf evo-2026-08-08.tar.gz -C /var/lib
sudo chown -R evo:evo /var/lib/evo && sudo chmod 0750 /var/lib/evo
sudo systemctl start evo
```

**Blobs depend on the backend.** With `"blobs": "local"` (the default) the PDF
bytes are inside that tarball. With `"blobs": {"s3": ...}` they are in the
bucket instead and the tarball is small — back the bucket up with versioning or
a lifecycle rule, and remember that the redb database is still the only record
of what those objects *are*. The index and page cache are always local, whatever
the backend, because they are memory-mapped files and object storage is not a
filesystem.

To skip what regenerates:

```sh
sudo tar -czf /root/evo-small.tar.gz -C /var/lib \
  --exclude='evo/library/pagecache' --exclude='evo/library/thumbs' \
  --exclude='evo/library/index' --exclude='evo/.local' evo
```

---

## 9. When something is wrong

**The service will not start.**

```sh
systemctl status evo
journalctl -u evo -n 50 --no-pager
```

- *"evo is already running and has this library open"* — something else holds
  the redb file. A stale `evo` process, or a second unit.
  `sudo pkill -u evo evo` and start again.
- *"could not listen on 127.0.0.1:8443"* — something else has the port:
  `sudo ss -lntp | grep 8443`.
- *Missing `auth.json`* — `evo serve init` has not been run (§3).
- *`GLIBC_2.xx not found`* — the binary was built against a newer glibc than
  this Debian has. Build on the box (Path A) or in a bookworm container.
- *`error while loading shared libraries: libgtk-3.so.0`* — the runtime
  packages are missing; re-run `install.sh` without `--no-deps`.

**Sign-in fails.**

- The same message appears whether the password or the code was wrong, on
  purpose — it does not say which.
- "That code has already been used" means exactly that: wait for the next one.
- Too many attempts locks the address out for five minutes.
- If the box's clock has drifted, TOTP breaks. `timedatectl` — `System clock
  synchronized: yes` is what you want; `sudo apt-get install -y systemd-timesyncd`
  if it is not.
- Locked out completely: `sudo rm /var/lib/evo/serve/auth.json` and run
  `evo serve init` again. That un-enrols the authenticator and signs out every
  device; it does not touch the library.

**The phone reaches nothing.**

```sh
curl -s http://127.0.0.1:8443/api/health        # on the box: is evo up?
tailscale status                                # tier (a): are both online?
sudo tailscale serve status                     # tier (a): is the mapping there?
sudo systemctl status cloudflared               # tier (b)
sudo caddy validate --config /etc/caddy/Caddyfile   # tier (c)
```

Remember the server binds loopback only. If `curl` works on the box and the
phone sees nothing, the problem is entirely in the tier, not in evo.

**Chat is slow or silent.**

```sh
curl -s http://127.0.0.1:8443/api/status        # needs a session cookie
journalctl -fu evo
```

The startup line names the model. If it says `built-in` and no weights have
been downloaded, that is the answer. Generations are serialised one at a time
on purpose — a second question waits for the first. If the wait is minutes
rather than seconds, §5 step 3.

**Search finds nothing after an upload.** Indexing is a background job; a
scanned page goes through OCR first, which is slow. `/api/status` reports
`index.pending` and the OCR counters.

**A document will not upload.** The limit is `max_upload_mb` (200 by default).
Encrypted PDFs are not supported at all.
