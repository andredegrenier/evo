#!/usr/bin/env bash
#
# install.sh -- put `evo serve` on a headless Debian box.
#
# Idempotent: every step checks the world before changing it, so running this
# again after a rebuild only replaces what actually differs. Nothing here talks
# to the network except `apt-get`, and nothing is started until you say so.
#
# The short version:
#
#   sudo ./install.sh --binary /path/to/evo          # installs binary + unit
#   sudo systemctl enable --now evo                  # after `serve init`
#
# See RUNBOOK.md for the whole story, including how to get the binary here.

set -euo pipefail

# ---------------------------------------------------------------- settings --

SERVICE_USER="evo"
SERVICE_GROUP="evo"
DATA_DIR="/var/lib/evo"
BIN_DEST="/usr/local/bin/evo"
UNIT_DEST="/etc/systemd/system/evo.service"

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# deploy/debian -> the repository root, when this was run out of a source tree.
SOURCE_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

BINARY=""
INSTALL_RUNTIME_DEPS=1
INSTALL_BUILD_DEPS=0
RUN_SMOKE=1
RUN_INIT=1
ASSUME_YES=0

# Runtime libraries the binary links even though `evo serve` opens no window:
# eframe and rfd are compiled in. `libgtk-3-0` was renamed `libgtk-3-0t64` in
# the 64-bit-time_t transition (Debian 13, Ubuntu 24.04), so both names are
# offered and whichever the archive has is the one installed.
RUNTIME_DEPS=("libgtk-3-0t64|libgtk-3-0" "libxkbcommon0" "libwayland-client0" "ca-certificates")

# What a native build needs (Path A). The same list CI installs on Linux, plus
# the C++ toolchain llama.cpp is built with.
BUILD_DEPS=("build-essential" "cmake" "clang" "pkg-config" "libgtk-3-dev" "libxkbcommon-dev" "libwayland-dev" "curl" "git")

# --------------------------------------------------------------- utilities --

say()  { printf '\033[1m==\033[0m %s\n' "$*"; }
info() { printf '   %s\n' "$*"; }
warn() { printf '\033[33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31mxx\033[0m %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

confirm() {
    # Anything non-interactive answers no, so an unattended run never blocks.
    [ "$ASSUME_YES" -eq 1 ] && return 0
    [ -t 0 ] || return 1
    local reply=""
    read -r -p "   $1 [y/N] " reply || return 1
    [[ "$reply" =~ ^[Yy] ]]
}

usage() {
    cat <<'EOF'
usage: sudo ./install.sh [options]

  --binary <path>   the evo binary to install (default: the newest of
                    <repo>/target/release/evo, <repo>/target/docker-amd64/release/evo,
                    or ./evo next to this script)
  --data-dir <path> where the library and the server's files live (default /var/lib/evo)
  --user <name>     the system account to run as (default evo)
  --build-deps      also apt-install the toolchain for a native build (Path A)
  --no-deps         install no packages at all
  --no-smoke        skip the model timing measurement
  --no-init         do not offer to run `evo serve init`
  --yes             answer yes to every prompt (non-interactive)
  -h, --help        this

Re-runnable: it replaces what differs and leaves the rest alone.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --binary)   BINARY="${2:?--binary needs a path}"; shift 2 ;;
        --binary=*) BINARY="${1#*=}"; shift ;;
        --data-dir)   DATA_DIR="${2:?--data-dir needs a path}"; shift 2 ;;
        --data-dir=*) DATA_DIR="${1#*=}"; shift ;;
        --user)   SERVICE_USER="${2:?--user needs a name}"; SERVICE_GROUP="$SERVICE_USER"; shift 2 ;;
        --user=*) SERVICE_USER="${1#*=}"; SERVICE_GROUP="$SERVICE_USER"; shift ;;
        --build-deps) INSTALL_BUILD_DEPS=1; shift ;;
        --no-deps)    INSTALL_RUNTIME_DEPS=0; INSTALL_BUILD_DEPS=0; shift ;;
        --no-smoke)   RUN_SMOKE=0; shift ;;
        --no-init)    RUN_INIT=0; shift ;;
        --yes|-y)     ASSUME_YES=1; shift ;;
        -h|--help)    usage; exit 0 ;;
        *) usage >&2; die "unknown option $1" ;;
    esac
done

MODEL_HOME="$DATA_DIR/.local/share"
MODEL_DIR="$MODEL_HOME/evo/library/models/llm"

# ------------------------------------------------------------- preflight 1 --
# Who and where.

preflight_host() {
    say "Checking the machine"

    [ "$(id -u)" -eq 0 ] || die "run this with sudo: it creates a user, writes /usr/local/bin and installs a systemd unit."

    if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        info "OS: ${PRETTY_NAME:-unknown}"
        case "${ID:-}${ID_LIKE:-}" in
            *debian*|*ubuntu*) : ;;
            *) warn "this installer assumes Debian or a derivative; package names may differ here." ;;
        esac
    else
        warn "no /etc/os-release; assuming Debian."
    fi

    have systemctl || die "no systemctl found. This installs a systemd service; a non-systemd host needs its own supervisor."

    local arch
    arch="$(uname -m)"
    info "Architecture: $arch"
    case "$arch" in
        x86_64|aarch64) : ;;
        *) warn "$arch is not a platform evo is built for; expect to build from source." ;;
    esac
}

# ------------------------------------------------------------- preflight 2 --
# CPU and memory: whether the built-in model is a good idea here at all.

HAS_AVX2=0
HAS_AVX512=0
MEM_TOTAL_GB=0
MEM_AVAIL_GB=0

preflight_hardware() {
    say "Checking the hardware the model would run on"

    local flags
    flags="$(grep -o 'avx2\|avx512f' /proc/cpuinfo 2>/dev/null | sort -u | tr '\n' ' ' || true)"
    [[ "$flags" == *avx2* ]] && HAS_AVX2=1
    [[ "$flags" == *avx512f* ]] && HAS_AVX512=1

    local model_name cores
    model_name="$(awk -F': ' '/^model name/ {print $2; exit}' /proc/cpuinfo 2>/dev/null || true)"
    cores="$(nproc 2>/dev/null || echo '?')"
    info "CPU: ${model_name:-unknown} (${cores} threads)"
    info "SIMD: ${flags:-none of avx2/avx512f}"

    if [ "$HAS_AVX2" -eq 0 ]; then
        warn "no AVX2. llama.cpp falls back to scalar kernels -- that is Sandy Bridge-era speed,"
        warn "which in practice means minutes per answer. Plan on an external endpoint."
    elif [ "$HAS_AVX512" -eq 1 ]; then
        info "AVX-512 present: the fastest of the CPU paths."
    fi

    # /proc/meminfo rather than `free -g`, which rounds a 7.6 GB machine to 7
    # and a 3.9 GB one to 3. MemAvailable is the number that matters: it is what
    # the kernel thinks can be handed out without swapping.
    local total_kb avail_kb
    total_kb="$(awk '/^MemTotal:/ {print $2; exit}' /proc/meminfo)"
    avail_kb="$(awk '/^MemAvailable:/ {print $2; exit}' /proc/meminfo)"
    MEM_TOTAL_GB=$(( total_kb / 1024 / 1024 ))
    MEM_AVAIL_GB=$(( avail_kb / 1024 / 1024 ))
    info "RAM: ${MEM_TOTAL_GB} GB total, ${MEM_AVAIL_GB} GB available"

    local free_gb
    free_gb="$(df -BG --output=avail "$(dirname "$DATA_DIR")" 2>/dev/null | tail -1 | tr -dc '0-9' || echo 0)"
    [ -n "$free_gb" ] || free_gb=0
    info "Disk free on $(dirname "$DATA_DIR"): ${free_gb} GB"
    if [ "$free_gb" -lt 6 ]; then
        warn "under 6 GB free. The 4B model alone is 2.5 GB, and page caches grow."
    fi
}

recommend_model() {
    say "Model recommendation for this box"
    if [ "$HAS_AVX2" -eq 0 ]; then
        info "-> Use an EXTERNAL endpoint. Without AVX2 the built-in model is not worth the wait."
        info "   RUNBOOK.md, 'Model fallback chain', step 3 (Ollama on your Mac over Tailscale)."
    elif [ "$MEM_TOTAL_GB" -lt 4 ]; then
        info "-> Use an EXTERNAL endpoint. Under 4 GB of RAM even the 1.7B model will fight the page cache."
    elif [ "$MEM_TOTAL_GB" -lt 6 ]; then
        info "-> Use qwen3-1.7b (1.1 GB), or an external endpoint."
        info "   sudo -u $SERVICE_USER env HOME=$DATA_DIR $BIN_DEST fetch-model qwen3-1.7b"
        info "   Note: 1.7B is a hybrid-thinking model. It reasons out loud, so answers"
        info "   begin with its thinking rather than with the answer."
    else
        info "-> Try the built-in qwen3-4b-instruct-2507 (2.5 GB download, ~4 GB resident)."
        info "   sudo -u $SERVICE_USER env HOME=$DATA_DIR $BIN_DEST fetch-model"
        info "   If the smoke test below is slow, fall back to qwen3-1.7b or an external endpoint."
    fi
}

# ---------------------------------------------------------------- packages --

apt_have_pkg() {
    dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q "ok installed"
}

apt_pkg_exists() {
    apt-cache show "$1" >/dev/null 2>&1
}

# Turn "a|b" into whichever of a, b the archive knows about.
resolve_pkg() {
    local spec="$1" candidate
    IFS='|' read -r -a __candidates <<< "$spec"
    for candidate in "${__candidates[@]}"; do
        if apt_have_pkg "$candidate" || apt_pkg_exists "$candidate"; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

install_packages() {
    local what="$1"; shift
    local specs=("$@")
    local wanted=() missing=() spec pkg

    for spec in "${specs[@]}"; do
        if pkg="$(resolve_pkg "$spec")"; then
            wanted+=("$pkg")
        else
            warn "no package matching '${spec//|/ or }' in this archive; skipping."
        fi
    done

    for pkg in "${wanted[@]}"; do
        apt_have_pkg "$pkg" || missing+=("$pkg")
    done

    if [ "${#missing[@]}" -eq 0 ]; then
        info "$what dependencies: already installed."
        return 0
    fi

    info "$what dependencies to install: ${missing[*]}"
    DEBIAN_FRONTEND=noninteractive apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${missing[@]}"
}

do_packages() {
    have apt-get || { warn "no apt-get; skipping package installation."; return 0; }
    if [ "$INSTALL_RUNTIME_DEPS" -eq 1 ]; then
        say "Runtime dependencies"
        install_packages "runtime" "${RUNTIME_DEPS[@]}"
    fi
    if [ "$INSTALL_BUILD_DEPS" -eq 1 ]; then
        say "Build dependencies (Path A: building on this box)"
        install_packages "build" "${BUILD_DEPS[@]}"
        info "Rust itself is not an apt package here. Install it as the build user:"
        info "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
        info "Then: cargo build --release --features s3"
        info "The first build compiles llama.cpp. On an old Intel chip expect 20-60 minutes."
    fi
}

# --------------------------------------------------------- user and layout --

do_user() {
    say "System account"
    if getent group "$SERVICE_GROUP" >/dev/null; then
        info "group $SERVICE_GROUP already exists."
    else
        groupadd --system "$SERVICE_GROUP"
        info "created system group $SERVICE_GROUP."
    fi

    if getent passwd "$SERVICE_USER" >/dev/null; then
        info "user $SERVICE_USER already exists."
    else
        # --system: no password ageing, no mail spool, a uid below 1000.
        # The home directory is the data directory on purpose: evo keeps model
        # weights under the platform data dir ($HOME/.local/share/evo), which
        # --data-dir does not move. The account is never logged into, so the
        # home directory is not created here -- do_dirs does it with the mode
        # the service wants.
        useradd --system --gid "$SERVICE_GROUP" --home-dir "$DATA_DIR" \
                --no-create-home --shell /usr/sbin/nologin \
                --comment "evo serve" "$SERVICE_USER"
        info "created system user $SERVICE_USER (home $DATA_DIR, no shell)."
    fi
}

do_dirs() {
    say "Data directory"
    if [ -d "$DATA_DIR" ]; then
        info "$DATA_DIR exists."
    else
        install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0750 "$DATA_DIR"
        info "created $DATA_DIR."
    fi

    # 0750 and evo:evo, always: a session file and an argon2 hash live here.
    local mode owner
    mode="$(stat -c '%a' "$DATA_DIR")"
    owner="$(stat -c '%U:%G' "$DATA_DIR")"
    if [ "$mode" != "750" ]; then
        chmod 0750 "$DATA_DIR"
        info "tightened $DATA_DIR from $mode to 750."
    fi
    if [ "$owner" != "$SERVICE_USER:$SERVICE_GROUP" ]; then
        chown "$SERVICE_USER:$SERVICE_GROUP" "$DATA_DIR"
        info "changed owner of $DATA_DIR from $owner to $SERVICE_USER:$SERVICE_GROUP."
    fi

    # Where `evo fetch-model` will put weights when it runs as this account.
    install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0750 "$MODEL_DIR"
}

# ------------------------------------------------------------------ binary --

find_binary() {
    [ -n "$BINARY" ] && return 0
    local candidate
    for candidate in \
        "$SOURCE_DIR/target/release/evo" \
        "$SOURCE_DIR/target/docker-amd64/release/evo" \
        "$SCRIPT_DIR/evo"
    do
        if [ -x "$candidate" ]; then
            BINARY="$candidate"
            return 0
        fi
    done
    return 1
}

check_binary() {
    file "$BINARY" 2>/dev/null | grep -q 'ELF' || warn "$BINARY does not look like an ELF binary."

    # `evo serve --help` prints the usage and exits non-zero (it is an error
    # path in the argument parser), so the exit status is not the signal --
    # the text is.
    local out
    out="$("$BINARY" serve --help 2>&1 || true)"
    if [[ "$out" == *"usage: evo serve"* ]]; then
        info "the binary runs here and answers 'evo serve --help'."
    else
        warn "'$BINARY serve --help' did not print the expected usage. Output was:"
        printf '%s\n' "$out" | sed 's/^/     /' >&2
        warn "If this is a GLIBC error, the binary was built against a newer glibc than this"
        warn "Debian has. Build on the box (Path A) or in a bookworm container (Path B)."
        die "refusing to install a binary that will not start."
    fi
}

do_binary() {
    say "Binary"
    if ! find_binary; then
        die "no evo binary found. Pass --binary <path>, or build one first (RUNBOOK.md, Path A or B)."
    fi
    info "source: $BINARY"
    check_binary

    if [ -x "$BIN_DEST" ] && cmp -s "$BINARY" "$BIN_DEST"; then
        info "$BIN_DEST is already this binary."
        return 0
    fi
    if [ -e "$BIN_DEST" ]; then
        info "replacing $BIN_DEST (a running service keeps the old inode until restarted)."
    fi
    # install(1) writes a new file and renames it into place, so a running
    # process is never handed a half-copied executable.
    install -m 0755 -o root -g root "$BINARY" "$BIN_DEST"
    info "installed $BIN_DEST."
    info "A running service keeps executing the old file until you restart it:"
    info "  sudo systemctl restart evo"
}

# ------------------------------------------------------------------- unit --

do_unit() {
    say "systemd unit"
    local src="$SCRIPT_DIR/evo.service"
    [ -f "$src" ] || die "$src is missing; run this script from the deploy/debian directory."

    if [ "$DATA_DIR" != "/var/lib/evo" ] || [ "$SERVICE_USER" != "evo" ]; then
        warn "the shipped unit hard-codes /var/lib/evo and User=evo. You asked for"
        warn "$DATA_DIR / $SERVICE_USER -- edit $UNIT_DEST after this, or use a drop-in."
    fi

    if [ -f "$UNIT_DEST" ] && cmp -s "$src" "$UNIT_DEST"; then
        info "$UNIT_DEST is already up to date."
    else
        install -m 0644 -o root -g root "$src" "$UNIT_DEST"
        systemctl daemon-reload
        info "installed $UNIT_DEST and reloaded systemd."
    fi

    if systemctl is-enabled --quiet evo 2>/dev/null; then
        info "the evo service is enabled."
    else
        info "not enabled yet -- it would fail to start before 'evo serve init' has run."
    fi
}

# ------------------------------------------------------------- smoke test --
#
# The honest hardware question is not "does this CPU have AVX2" but "how long
# does this box take to answer". Estimates lie; a measurement does not.
#
# There is no `evo generate` subcommand to time -- the binary's entry points are
# `serve`, `mcp-serve` and `fetch-model`, and every generation path is behind
# either the GUI or an authenticated HTTP route. What there IS, in the source
# tree, is an #[ignore]d test that loads a downloaded model and streams a real
# completion. On a Path A box the source tree is right here, so that test is the
# measurement. Without a source tree we print the instructions and say so.

SMOKE_TEST="llm::backend::tests::the_builtin_backend_answers_and_streams"

readable_by() {
    # `sudo -u x test -r f` needs /usr/bin/test to exist; going through a shell
    # is one fewer assumption about the box.
    sudo -u "$1" bash -c "test -r '$2'" 2>/dev/null
}

find_model_file() {
    # Prints "<xdg_data_home>|<gguf path>|<catalogue id>" for the first model
    # found, in preference order. The id matters: the test takes an id, not a
    # path, and resolves it under $XDG_DATA_HOME (or $HOME/.local/share).
    #
    # $1, if given, is a user the file must be readable by -- the model the
    # service uses lives under 0750 /var/lib/evo, which whoever owns the source
    # tree cannot open.
    local as_user="${1:-}"
    local home ids id file
    local homes=("$MODEL_HOME")
    if [ -n "${SUDO_USER:-}" ]; then
        homes+=("$(getent passwd "$SUDO_USER" | cut -d: -f6)/.local/share")
    fi
    ids=("qwen3-4b-instruct-2507:qwen3-4b-instruct-2507-q4_k_m.gguf" "qwen3-1.7b:qwen3-1.7b-q4_k_m.gguf")

    for home in "${homes[@]}"; do
        [ -n "$home" ] || continue
        for id in "${ids[@]}"; do
            file="$home/evo/library/models/llm/${id#*:}"
            [ -f "$file" ] || continue
            if [ -n "$as_user" ] && ! readable_by "$as_user" "$file"; then
                continue
            fi
            printf '%s|%s|%s' "$home" "$file" "${id%%:*}"
            return 0
        done
    done
    return 1
}

smoke_instructions() {
    local id="${1:-qwen3-4b-instruct-2507}" home="${2:-$MODEL_HOME}"
    cat <<EOF
   evo's binary has no one-shot 'generate' subcommand to time, so the
   measurement lives in the source tree instead. From a checkout on THIS box:

     XDG_DATA_HOME=$home EVO_LLM_TEST_MODEL=$id \\
       cargo test --release --bin evo -- --ignored --exact --nocapture \\
       $SMOKE_TEST

   Time it (\`time\`, or the harness's own line). That figure is model load plus
   a short real generation, cold.

   If the weights belong to $SERVICE_USER (they do, under 0750 $DATA_DIR), run the
   command under sudo so it can read them, and hand the build tree back after:

     sudo -E env XDG_DATA_HOME=$home EVO_LLM_TEST_MODEL=$id cargo test ...
     sudo chown -R "\$(id -un)" target

   Otherwise: start the service, ask one question from the phone, and watch
   \`journalctl -fu evo\`.
EOF
}

do_smoke() {
    say "Model smoke test"
    if [ "$RUN_SMOKE" -eq 0 ]; then
        info "skipped (--no-smoke)."
        return 0
    fi

    local found home file id
    if ! found="$(find_model_file)"; then
        info "no model file on this box yet, so there is nothing to time."
        info "Download one first (see the recommendation above), then re-run this script."
        return 0
    fi
    home="${found%%|*}"; file="${found#*|}"; id="${file#*|}"; file="${file%%|*}"
    info "model: $file ($(du -h "$file" | cut -f1))"

    if [ ! -f "$SOURCE_DIR/Cargo.toml" ]; then
        warn "no source tree next to this script, so the timing cannot be run here."
        smoke_instructions "$id" "$home"
        return 0
    fi

    # Run as whoever owns the tree: cargo writes to target/, and root-owned
    # object files in a user's checkout are a nasty parting gift.
    local runner
    runner="$(stat -c '%U' "$SOURCE_DIR/Cargo.toml")"
    if ! sudo -u "$runner" -H bash -lc 'command -v cargo >/dev/null'; then
        warn "$runner has no cargo on PATH, so the timing cannot be run here."
        smoke_instructions "$id" "$home"
        return 0
    fi

    # The service's copy is under 0750 /var/lib/evo and $runner cannot open it.
    # Look again for one that they can -- a second copy in their own data
    # directory is the ordinary case on a box where somebody has been building.
    if ! readable_by "$runner" "$file"; then
        local readable
        if readable="$(find_model_file "$runner")"; then
            home="${readable%%|*}"; file="${readable#*|}"; id="${file#*|}"; file="${file%%|*}"
            info "using the copy $runner can read: $file"
        else
            warn "$runner cannot read $file (it belongs to $SERVICE_USER, and $DATA_DIR is 0750)."
            smoke_instructions "$id" "$home"
            return 0
        fi
    fi

    if ! confirm "Time a real generation now? It compiles the test binary first."; then
        info "skipped."
        smoke_instructions "$id" "$home"
        return 0
    fi

    local started ended elapsed status=0
    started="$(date +%s)"
    sudo -u "$runner" -H bash -lc \
        "cd '$SOURCE_DIR' && XDG_DATA_HOME='$home' EVO_LLM_TEST_MODEL='$id' \
         cargo test --release --bin evo -- --ignored --exact --nocapture $SMOKE_TEST" \
        || status=$?
    ended="$(date +%s)"
    elapsed=$(( ended - started ))

    if [ "$status" -ne 0 ]; then
        warn "the generation failed (exit $status). The model is present but this build cannot run it."
        return 0
    fi

    info "Wall time including compilation: ${elapsed}s."
    info "The number to read is the test harness's own duration, printed above:"
    info "that is model load plus a short real completion, from cold."
    cat <<'EOF'
   Rough reading of that figure, for a first answer on a cold cache:
     under 20s   comfortable -- keep the built-in model
     20-60s      usable if you are patient; consider qwen3-1.7b
     over 60s    point the config at an external endpoint instead
   Every later answer skips the load, so steady-state chat is faster than this.
EOF
}

# -------------------------------------------------------------------- init --

do_init() {
    say "Credentials"
    local auth="$DATA_DIR/serve/auth.json"
    if [ -f "$auth" ]; then
        info "$auth exists; the password and authenticator are already set up."
        info "To start over: rm $auth (this un-enrols your app and signs out every device)."
        return 0
    fi

    info "Nothing is set up yet. \`evo serve init\` asks for a password and prints an"
    info "otpauth:// URI to add to an authenticator app -- over this SSH session, so"
    info "nobody needs a screen on the box."
    echo
    info "It runs as $SERVICE_USER so the files it writes belong to the service:"
    info "  sudo -u $SERVICE_USER env HOME=$DATA_DIR $BIN_DEST serve init --data-dir $DATA_DIR"
    echo
    warn "The password is echoed as you type (evo has no terminal crate). To avoid that:"
    warn "  sudo -u $SERVICE_USER env HOME=$DATA_DIR EVO_SERVE_PASSWORD='...' $BIN_DEST serve init --data-dir $DATA_DIR"
    warn "  (and then clear it from your shell history)"
    echo

    if [ "$RUN_INIT" -eq 0 ]; then
        info "skipped (--no-init)."
        return 0
    fi
    if confirm "Run \`evo serve init\` now?"; then
        sudo -u "$SERVICE_USER" env HOME="$DATA_DIR" XDG_DATA_HOME="$MODEL_HOME" \
            "$BIN_DEST" serve init --data-dir "$DATA_DIR"
    else
        info "not run. Do it before enabling the service -- it will not start without auth.json."
    fi
}

# ------------------------------------------------------------------- next --

do_next_steps() {
    say "Next"
    cat <<EOF
   1. If you have not yet:  sudo -u $SERVICE_USER env HOME=$DATA_DIR $BIN_DEST serve init --data-dir $DATA_DIR
   2. Start it:             sudo systemctl enable --now evo
   3. Watch it:             journalctl -fu evo
   4. Prove it locally:     curl -s http://127.0.0.1:8443/api/health
   5. Reach it from the phone: RUNBOOK.md, "Getting to it from the phone".
      Tailscale is the recommended tier -- a valid HTTPS certificate, no open
      ports, and no Caddy at all.

   The service binds 127.0.0.1 only. Until you set up one of those three tiers,
   nothing outside this box can reach it -- which is the intended order.
EOF
}

# ------------------------------------------------------------------- main --

main() {
    preflight_host
    preflight_hardware
    do_packages
    do_user
    do_dirs
    do_binary
    do_unit
    recommend_model
    do_smoke
    do_init
    do_next_steps
}

main "$@"
