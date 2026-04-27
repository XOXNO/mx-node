#!/bin/sh
# mxnode installer — single-line install for any host.
#
# Usage:
#     curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh
#
# Options (pass after `sh -s --`):
#     --version <TAG>   Install a specific release (default: latest)
#     --dir <PATH>      Install dir (default: /usr/local/bin)
#     --force           Reinstall even if the requested version is already present
#     --help            Show this message
#
# Environment overrides (handy for CI / unattended installs):
#     MXNODE_INSTALL_DIR    same as --dir
#     MXNODE_VERSION        same as --version (overridden by --version flag)
#     MXNODE_GITHUB_TOKEN   GitHub token used for the `latest` API lookup, dodges
#                           the unauthenticated 60 req/h rate limit
#     MXNODE_FORCE=1        same as --force
#
# Examples:
#     curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh
#     curl -fsSL .../install.sh | sh -s -- --version v0.1.0
#     curl -fsSL .../install.sh | sh -s -- --dir "$HOME/.local/bin"
#
# What it does:
#     1. detects OS + CPU (darwin/linux × x86_64/aarch64)
#     2. resolves the release tag (latest if --version omitted)
#     3. short-circuits if the same version is already installed
#     4. downloads the matching release tarball + SHA256SUMS
#     5. verifies sha256, extracts, installs the binary
#     6. asserts the freshly-installed binary reports the expected version
#
# POSIX sh — runs under busybox / dash on minimal images.

set -eu

REPO="XOXNO/mx-node"
INSTALL_DIR="${MXNODE_INSTALL_DIR:-/usr/local/bin}"
VERSION="${MXNODE_VERSION:-latest}"
FORCE="${MXNODE_FORCE:-0}"

# ── argument parse ────────────────────────────────────────────────
while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            [ $# -ge 2 ] || { echo "--version needs a value" >&2; exit 1; }
            VERSION="$2"
            shift 2
            ;;
        --dir)
            [ $# -ge 2 ] || { echo "--dir needs a value" >&2; exit 1; }
            INSTALL_DIR="$2"
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        --help|-h)
            cat <<'EOF'
mxnode installer

  curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh

Options:
  --version <TAG>   Install a specific release (default: latest)
  --dir <PATH>      Install dir (default: /usr/local/bin)
  --force           Reinstall even if the requested version is already present
  --help            Show this message

Env overrides: MXNODE_INSTALL_DIR, MXNODE_VERSION, MXNODE_GITHUB_TOKEN, MXNODE_FORCE
EOF
            exit 0
            ;;
        *)
            echo "unknown argument: $1 (see --help)" >&2
            exit 1
            ;;
    esac
done

# ── platform detect ───────────────────────────────────────────────
uname_s="$(uname -s 2>/dev/null || echo Unknown)"
uname_m="$(uname -m 2>/dev/null || echo Unknown)"

case "$uname_s" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-musl" ;;
    *)
        echo "unsupported OS: $uname_s" >&2
        echo "mxnode currently ships binaries for macOS + Linux." >&2
        echo "Build from source: https://github.com/$REPO" >&2
        exit 1
        ;;
esac

case "$uname_m" in
    x86_64|amd64) arch_part="x86_64" ;;
    arm64|aarch64) arch_part="aarch64" ;;
    *)
        echo "unsupported CPU: $uname_m" >&2
        echo "Build from source: https://github.com/$REPO" >&2
        exit 1
        ;;
esac

target="${arch_part}-${os_part}"

# ── tools ─────────────────────────────────────────────────────────
need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing required tool: $1" >&2
        echo "install it via your package manager and re-run." >&2
        exit 1
    }
}
need curl
need tar

# Pick whichever sha256 tool is available — both common on Linux/macOS.
if command -v sha256sum >/dev/null 2>&1; then
    sha256_cmd='sha256sum'
elif command -v shasum >/dev/null 2>&1; then
    sha256_cmd='shasum -a 256'
else
    echo "no sha256sum or shasum found — refusing to install unverified binary" >&2
    exit 1
fi

# Build the curl auth header once. MXNODE_GITHUB_TOKEN dodges the
# anonymous 60 req/h limit on api.github.com — releases archives at
# objects.githubusercontent.com aren't rate-limited so the header
# is only used for the `latest` lookup.
auth_header=""
if [ -n "${MXNODE_GITHUB_TOKEN:-}" ]; then
    auth_header="Authorization: Bearer ${MXNODE_GITHUB_TOKEN}"
fi

# ── resolve version ───────────────────────────────────────────────
if [ "$VERSION" = "latest" ]; then
    echo "→ resolving latest release of $REPO..."
    api_url="https://api.github.com/repos/${REPO}/releases/latest"
    # `tag_name` is the first key in the JSON response shape — grep
    # the first occurrence to avoid pulling in jq just for one field.
    if [ -n "$auth_header" ]; then
        latest_json="$(curl -fsSL -H "$auth_header" "$api_url")" || latest_json=""
    else
        latest_json="$(curl -fsSL "$api_url")" || latest_json=""
    fi
    VERSION="$(printf '%s\n' "$latest_json" \
        | grep -m1 '"tag_name"' \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/' || true)"
    if [ -z "$VERSION" ]; then
        echo "could not resolve latest release. Try --version vX.Y.Z." >&2
        echo "(rate-limited? export MXNODE_GITHUB_TOKEN=<a github token>)" >&2
        exit 1
    fi
fi

# ── short-circuit if already installed at the requested version ───
existing="${INSTALL_DIR}/mxnode"
if [ "$FORCE" != "1" ] && [ -x "$existing" ]; then
    # `mxnode --version` prints `mxnode <semver>` — the version line
    # itself is whatever the binary's clap derives from Cargo.toml,
    # without the leading `v`. Compare both with and without `v` so
    # a tag of `v0.8.4` matches a binary that prints `0.8.4`.
    current_raw="$("$existing" --version 2>/dev/null | awk 'NR==1 {print $2}' || true)"
    requested_strip="${VERSION#v}"
    if [ -n "$current_raw" ] && [ "$current_raw" = "$requested_strip" ]; then
        echo "✓ mxnode ${VERSION} is already installed at ${existing}"
        echo "  pass --force to reinstall."
        exit 0
    fi
fi

archive="mxnode-${VERSION}-${target}.tar.gz"
archive_url="https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
sums_url="https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS"
release_url="https://github.com/${REPO}/releases/tag/${VERSION}"

echo "→ installing mxnode ${VERSION} for ${target}"
echo "  source:  ${release_url}"
echo "  archive: ${archive_url}"

# ── download ──────────────────────────────────────────────────────
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t mxnode)"
trap 'rm -rf "$tmp"' EXIT INT TERM

curl -fsSL "$archive_url" -o "${tmp}/${archive}" || {
    echo "failed to download $archive_url" >&2
    echo "verify the release exists at https://github.com/${REPO}/releases" >&2
    exit 1
}
curl -fsSL "$sums_url" -o "${tmp}/SHA256SUMS" || {
    echo "failed to download SHA256SUMS" >&2
    exit 1
}

# ── verify ────────────────────────────────────────────────────────
echo "→ verifying sha256..."
expected="$(grep " ${archive}\$" "${tmp}/SHA256SUMS" | awk '{print $1}')"
if [ -z "$expected" ]; then
    echo "no sha256 entry for $archive in SHA256SUMS" >&2
    echo "the release may be incomplete; report at https://github.com/${REPO}/issues" >&2
    exit 1
fi
actual="$( $sha256_cmd "${tmp}/${archive}" | awk '{print $1}')"
if [ "$expected" != "$actual" ]; then
    echo "sha256 mismatch!" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    exit 1
fi
echo "  ok ($expected)"

# ── extract ───────────────────────────────────────────────────────
echo "→ extracting..."
tar -xzf "${tmp}/${archive}" -C "${tmp}"
[ -x "${tmp}/mxnode" ] || {
    echo "expected mxnode binary missing from archive" >&2
    exit 1
}

# ── install ───────────────────────────────────────────────────────
mkdir -p "$INSTALL_DIR" 2>/dev/null || true
if [ -w "$INSTALL_DIR" ] || [ "$(id -u)" = "0" ]; then
    install -m 0755 "${tmp}/mxnode" "${INSTALL_DIR}/mxnode"
elif command -v sudo >/dev/null 2>&1; then
    echo "→ installing to ${INSTALL_DIR} (requires sudo)"
    sudo install -m 0755 "${tmp}/mxnode" "${INSTALL_DIR}/mxnode"
else
    echo "${INSTALL_DIR} is not writable and sudo is unavailable" >&2
    echo "rerun with --dir \"\$HOME/.local/bin\" or another writable path" >&2
    exit 1
fi

# ── post-install verify ───────────────────────────────────────────
# Run the freshly-installed binary and confirm it prints the version
# we asked for. Catches a corrupted archive that somehow passed
# sha256 (extremely unlikely), an extraction race, or a stale binary
# from earlier on PATH shadowing the new one.
echo "→ verifying installed binary..."
installed_raw="$("${INSTALL_DIR}/mxnode" --version 2>/dev/null | awk 'NR==1 {print $2}' || true)"
requested_strip="${VERSION#v}"
if [ -z "$installed_raw" ]; then
    echo "installed binary failed to run \`mxnode --version\`" >&2
    echo "  path: ${INSTALL_DIR}/mxnode" >&2
    exit 1
fi
if [ "$installed_raw" != "$requested_strip" ]; then
    echo "installed binary reports version '${installed_raw}', expected '${requested_strip}'" >&2
    echo "  remove ${INSTALL_DIR}/mxnode and re-run with --force" >&2
    exit 1
fi

echo
echo "✓ installed mxnode ${VERSION} to ${INSTALL_DIR}/mxnode"
echo "  source:  ${release_url}"
echo "  sha256:  ${expected}"
echo
"${INSTALL_DIR}/mxnode" --version

# Friendly tail. mxnode auto-initialises its config (auto-detected
# $USER/$HOME, network=mainnet) on first state-changing command, so
# no separate init step is needed.
echo
echo "Next:"
echo "    mxnode install            # auto-init + build + install nodes"
echo "    mxnode --help"
echo
echo "Default network is mainnet. Switch with:"
echo "    mxnode config set network.environment testnet"
echo
echo "Path note: ensure ${INSTALL_DIR} is on your PATH."
case ":${PATH}:" in
    *:"${INSTALL_DIR}":*) ;;
    *)
        echo "  ${INSTALL_DIR} isn't currently on your PATH — add this to your shell rc:"
        echo "      export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac
