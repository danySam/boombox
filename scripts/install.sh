#!/usr/bin/env bash
# Fetch the right boombox binary for this machine, check it against the
# checksums published beside it, and put it somewhere on PATH.
#
# Lives in the repository so that what you are about to pipe into a shell
# can be read first. It verifies before it installs, needs no root, and
# refuses rather than guesses when this machine cannot run the binary.
set -euo pipefail

REPO="danySam/boombox"
PREFIX="${BOOMBOX_PREFIX:-$HOME/.local}"
VERSION=""

# The oldest glibc the released Linux binaries can run against. Measured,
# not guessed: they are built on Ubuntu 22.04 and need GLIBC_2.34.
MIN_GLIBC_MAJOR=2
MIN_GLIBC_MINOR=34

die() { printf 'boombox: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*" >&2; }

usage() {
    cat <<'EOF'
install.sh [--prefix DIR] [--version vX.Y.Z]

  --prefix DIR     install into DIR/bin (default: ~/.local)
  --version TAG    install this release rather than the latest
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) usage >&2; die "unknown option: $1" ;;
    esac
done

for tool in curl tar uname; do
    command -v "$tool" >/dev/null || die "$tool is needed and was not found"
done

# Whichever of the two this system calls its sha256 tool.
if command -v shasum >/dev/null; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
elif command -v sha256sum >/dev/null; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
    die "neither shasum nor sha256sum was found, so the download cannot be verified"
fi

case "$(uname -s)" in
    Darwin) os="apple-darwin" ;;
    Linux)  os="unknown-linux-gnu" ;;
    *) die "no released binary for $(uname -s). Build it with: cargo install boombox --locked --features streaming" ;;
esac

case "$(uname -m)" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64)  arch="x86_64" ;;
    *) die "no released binary for $(uname -m). Build it with: cargo install boombox --locked --features streaming" ;;
esac
target="${arch}-${os}"

# A binary links the glibc it was built against and runs on anything
# newer, so an older system has to be turned away here rather than at the
# first run, where it fails with a loader error that explains nothing.
if [ "$os" = "unknown-linux-gnu" ] && command -v ldd >/dev/null; then
    libc=$(ldd --version 2>&1 || true)

    # musl is not an older glibc, it is a different libc: the released
    # binaries name a loader this system does not have, and no package
    # it can install will change that.
    case "$libc" in
        *musl*)
            die "this system uses musl, and the released binaries are built \
against glibc.
       Install with cargo instead:
         cargo install boombox --locked --features streaming"
            ;;
    esac

    have=$(printf '%s\n' "$libc" | head -1 | grep -oE '[0-9]+\.[0-9]+' | tail -1 || true)
    if [ -n "$have" ]; then
        have_major=${have%%.*}
        have_minor=${have##*.}
        if [ "$have_major" -lt "$MIN_GLIBC_MAJOR" ] ||
           { [ "$have_major" -eq "$MIN_GLIBC_MAJOR" ] && [ "$have_minor" -lt "$MIN_GLIBC_MINOR" ]; }; then
            die "this system has glibc $have; the released binaries need \
${MIN_GLIBC_MAJOR}.${MIN_GLIBC_MINOR} or newer.
       Install with cargo instead:
         cargo install boombox --locked --features streaming"
        fi
    fi
fi

if [ -z "$VERSION" ]; then
    # Downloaded whole and read afterwards. Piping curl straight into a
    # grep that stops at the first match kills curl with EPIPE, which
    # under `pipefail` is indistinguishable from GitHub being down.
    latest=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest") ||
        die "could not ask GitHub for the latest release"
    VERSION=$(printf '%s\n' "$latest" | grep '"tag_name"' | cut -d'"' -f4)
    [ -n "$VERSION" ] || die "could not work out the latest release; pass --version"
fi

base="https://github.com/${REPO}/releases/download/${VERSION}"
archive="boombox-${VERSION}-${target}.tar.gz"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

note "boombox ${VERSION} for ${target}"
curl -fsSL "${base}/${archive}" -o "${work}/${archive}" ||
    die "could not download ${archive}. Check that ${VERSION} has a build for ${target}."
curl -fsSL "${base}/SHA256SUMS" -o "${work}/SHA256SUMS" ||
    die "could not download SHA256SUMS, so the archive cannot be verified"

# Compared by hand rather than with `shasum -c`, whose handling of a
# missing entry differs between the two tools this might be using. The
# name is anchored at both ends, so exactly one line can match.
want=$(grep -E "[ /]${archive}\$" "${work}/SHA256SUMS" | cut -d' ' -f1)
[ -n "$want" ] || die "SHA256SUMS has no entry for ${archive}"
got=$(sha256 "${work}/${archive}")
[ "$want" = "$got" ] || die "checksum mismatch for ${archive}
       expected ${want}
       got      ${got}"
note "checksum ok"

tar -xzf "${work}/${archive}" -C "$work"
unpacked="${work}/boombox-${VERSION}-${target}/boombox"
[ -f "$unpacked" ] || die "the archive did not contain the binary where expected"

mkdir -p "${PREFIX}/bin"
# Written beside the target and moved into place, so an install that
# fails halfway cannot leave a half-written boombox on PATH.
tmp="${PREFIX}/bin/.boombox.$$"
cp "$unpacked" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "${PREFIX}/bin/boombox"
note "installed ${PREFIX}/bin/boombox"

# ALSA is linked at startup, so without it the binary does not run at
# all. Said after installing, because it is a thing to go and do rather
# than a reason to stop.
if [ "$os" = "unknown-linux-gnu" ] && ! why=$("${PREFIX}/bin/boombox" --version 2>&1); then
    note ""
    note "it is installed, but it did not start:"
    printf '%s\n' "$why" | while IFS= read -r line; do note "  $line"; done
    note ""
    note "most often that is ALSA missing:"
    note "  sudo apt install libasound2      # Debian, Ubuntu"
    note "  sudo dnf install alsa-lib        # Fedora"
    exit 1
fi

case ":${PATH}:" in
    *":${PREFIX}/bin:"*) ;;
    *)
        note ""
        note "${PREFIX}/bin is not on your PATH. Add it:"
        note "  export PATH=\"${PREFIX}/bin:\$PATH\""
        ;;
esac

note ""
note "$("${PREFIX}/bin/boombox" --version 2>/dev/null || echo "boombox ${VERSION}")"
note "next: boombox setup"
