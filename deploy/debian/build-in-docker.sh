#!/usr/bin/env bash
#
# build-in-docker.sh -- Path B: build the Debian x86_64 `evo` binary on a Mac.
#
# Path A (build on the box itself) is the recommended one. This exists for when
# the box cannot host a Rust toolchain, or when you want the binary in hand
# before the box is reachable.
#
#   ./build-in-docker.sh              # -> target/docker-amd64/release/evo
#   ./build-in-docker.sh --shell      # a prompt inside the build container
#
# On Apple Silicon this runs an amd64 container under emulation. It works; it
# is markedly slower than a native build, and the llama.cpp C++ compile is
# where you will feel it -- reckon on the better part of an hour for a cold
# build. If the Debian box has any spare hours at all, Path A wins.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd -- "$SCRIPT_DIR/../.." && pwd)"

IMAGE="evo-build:bookworm-amd64"
PLATFORM="linux/amd64"
# A target directory of its own: the host's target/ holds macOS objects, and
# mixing the two only means both get rebuilt.
TARGET_SUBDIR="docker-amd64"
FEATURES="s3"
WANT_SHELL=0

usage() {
    cat <<'EOF'
usage: ./build-in-docker.sh [options]

  --platform <p>   target platform (default linux/amd64; use linux/arm64 for Graviton)
  --features <f>   cargo features (default s3; pass "" for none)
  --target-dir <d> subdirectory of target/ to build into (default docker-amd64)
  --shell          drop into a shell in the build container instead of building
  -h, --help       this
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --platform)   PLATFORM="${2:?--platform needs a value}"; shift 2 ;;
        --platform=*) PLATFORM="${1#*=}"; shift ;;
        --features)   FEATURES="${2-}"; shift 2 ;;
        --features=*) FEATURES="${1#*=}"; shift ;;
        --target-dir)   TARGET_SUBDIR="${2:?--target-dir needs a value}"; shift 2 ;;
        --target-dir=*) TARGET_SUBDIR="${1#*=}"; shift ;;
        --shell)      WANT_SHELL=1; shift ;;
        -h|--help)    usage; exit 0 ;;
        *) usage >&2; echo "unknown option $1" >&2; exit 1 ;;
    esac
done

command -v docker >/dev/null 2>&1 || {
    echo "docker is not on PATH. Install Docker Desktop (or colima), or use Path A." >&2
    exit 1
}
docker info >/dev/null 2>&1 || {
    echo "docker is installed but not running." >&2
    exit 1
}

if [ "$PLATFORM" = "linux/amd64" ] && [ "$(uname -m)" = "arm64" ]; then
    echo "Note: building linux/amd64 on Apple Silicon runs under emulation."
    echo "      Expect it to take a long time. Path A on the box is faster."
    echo
fi

echo "== Building the image ($PLATFORM)"
docker build --platform "$PLATFORM" -t "$IMAGE" -f "$SCRIPT_DIR/Dockerfile.build" "$SCRIPT_DIR"

# Named volumes for the registry and the git checkouts, so a second run does
# not re-download every crate. The target directory is a bind mount instead:
# the point of the exercise is a binary you can scp.
mkdir -p "$REPO_DIR/target/$TARGET_SUBDIR"

# -t only when there is a terminal to attach: with the flag on and no tty,
# docker refuses to start at all, which would break this under CI or nohup.
tty_flags=(-i)
if [ -t 0 ]; then
    tty_flags=(-i -t)
fi

docker_run=(
    docker run --rm "${tty_flags[@]}"
    --platform "$PLATFORM"
    -v "$REPO_DIR:/src"
    -v "evo-cargo-registry:/usr/local/cargo/registry"
    -v "evo-cargo-git:/usr/local/cargo/git"
    -e "CARGO_TARGET_DIR=/src/target/$TARGET_SUBDIR"
    -w /src
    "$IMAGE"
)

if [ "$WANT_SHELL" -eq 1 ]; then
    exec "${docker_run[@]}" bash
fi

echo "== Building evo"
if [ -n "$FEATURES" ]; then
    "${docker_run[@]}" cargo build --release --features "$FEATURES"
else
    "${docker_run[@]}" cargo build --release
fi

BINARY="$REPO_DIR/target/$TARGET_SUBDIR/release/evo"
[ -f "$BINARY" ] || { echo "the build finished but $BINARY is not there." >&2; exit 1; }

echo
echo "== Done"
echo "   $BINARY"
file "$BINARY" 2>/dev/null || true
echo
cat <<EOF
Copy it to the box and install it:

  scp $BINARY <you>@<box>:/tmp/evo
  ssh <you>@<box>
  sudo /path/to/deploy/debian/install.sh --binary /tmp/evo

The binary was built against Debian bookworm's glibc (2.36), so it runs on
Debian 12 and anything newer. It will NOT run on anything older -- if the box
says "GLIBC_2.36 not found", build on the box instead (Path A).
EOF
