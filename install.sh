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
#     --min             Install the bandwidth-optimised `-min` variant
#                       (~10-18% smaller, built with nightly Rust + `-Zbuild-std`
#                       + `-Cpanic=immediate-abort`; same functionality, shorter
#                       panic messages). Fails if the tag has no `-min` artefact —
#                       not all releases ship one because the nightly build job
#                       is `continue-on-error: true` upstream and may have skipped.
#     --help            Show this message
#
# Environment overrides (handy for CI / unattended installs):
#     MXNODE_INSTALL_DIR     same as --dir
#     MXNODE_VERSION         same as --version (overridden by --version flag)
#     MXNODE_FORCE=1         same as --force
#     MXNODE_VARIANT=min     same as --min
#     MXNODE_REPO            override GitHub repo (default: XOXNO/mx-node)
#     MXNODE_GITHUB_TOKEN    GitHub token used for the `latest` API lookup,
#                            dodges the unauthenticated 60 req/h rate limit
#     MXNODE_REQUIRE_COSIGN  set to 1 to fail when cosign is missing OR the
#                            release lacks signatures (default: best-effort,
#                            verify when both are available, sha256 otherwise)
#
# Examples:
#     curl -fsSL https://raw.githubusercontent.com/XOXNO/mx-node/main/install.sh | sh
#     curl -fsSL .../install.sh | sh -s -- --version v0.1.0
#     curl -fsSL .../install.sh | sh -s -- --dir "$HOME/.local/bin"
#     MXNODE_REQUIRE_COSIGN=1 curl -fsSL .../install.sh | sh   # strict: cosign mandatory
#
# What it does:
#     1. detects OS + CPU (darwin/linux × x86_64/aarch64)
#     2. resolves the release tag (latest if --version omitted)
#     3. short-circuits if the same version is already installed
#     4. downloads the matching release tarball + SHA256SUMS
#     5. verifies sha256
#     6. (if cosign is installed AND the release ships .sig/.pem files)
#        verifies the keyless cosign signature against the canonical
#        GitHub Actions OIDC identity for the release.yml workflow
#     7. extracts, installs the binary, asserts the version it reports
#
# POSIX sh — runs under busybox / dash on minimal images. The entire
# script body lives inside `__mxnode_install_main` and is invoked once
# at the bottom; this prevents a truncated `curl | sh` pipe from
# executing partial commands (the function definition is a no-op until
# the final invoking line is read in full). Industry-standard pattern
# used by rustup, brew, nvm, get-helm-3, etc.

set -eu

__mxnode_install_main() {
    REPO="${MXNODE_REPO:-XOXNO/mx-node}"
    INSTALL_DIR="${MXNODE_INSTALL_DIR:-/usr/local/bin}"
    VERSION="${MXNODE_VERSION:-latest}"
    FORCE="${MXNODE_FORCE:-0}"
    REQUIRE_COSIGN="${MXNODE_REQUIRE_COSIGN:-0}"
    # Variant suffix appended to the archive name. `min` selects the
    # nightly + build-std artefact; empty string selects the canonical
    # stable artefact. Anything else is rejected to avoid silent typos
    # picking the wrong file.
    VARIANT="${MXNODE_VARIANT:-}"

    # ── argument parse ────────────────────────────────────────────
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
            --min)
                VARIANT=min
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

Env overrides: MXNODE_INSTALL_DIR, MXNODE_VERSION, MXNODE_GITHUB_TOKEN,
               MXNODE_FORCE, MXNODE_REPO, MXNODE_REQUIRE_COSIGN
EOF
                exit 0
                ;;
            *)
                echo "unknown argument: $1 (see --help)" >&2
                exit 1
                ;;
        esac
    done

    # ── platform detect ───────────────────────────────────────────
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

    # ── tools ─────────────────────────────────────────────────────
    need_tool() {
        command -v "$1" >/dev/null 2>&1 || {
            echo "missing required tool: $1" >&2
            echo "install it via your package manager and re-run." >&2
            exit 1
        }
    }
    need_tool curl
    need_tool tar

    # Pick whichever sha256 tool is available — both common on Linux/macOS.
    if command -v sha256sum >/dev/null 2>&1; then
        sha256_cmd='sha256sum'
    elif command -v shasum >/dev/null 2>&1; then
        sha256_cmd='shasum -a 256'
    else
        echo "no sha256sum or shasum found — refusing to install unverified binary" >&2
        exit 1
    fi

    # `curl --retry` plus `--retry-delay` add resilience against
    # transient CDN flaps and runner network blips. Three retries
    # with a 2s base delay (linear backoff under POSIX curl, expo
    # under newer ones) is the rustup convention.
    curl_args="-fsSL --retry 3 --retry-delay 2"

    # Build the curl auth header once. MXNODE_GITHUB_TOKEN dodges the
    # anonymous 60 req/h limit on api.github.com — releases archives at
    # objects.githubusercontent.com aren't rate-limited so the header
    # is only used for the `latest` lookup.
    auth_header=""
    if [ -n "${MXNODE_GITHUB_TOKEN:-}" ]; then
        auth_header="Authorization: Bearer ${MXNODE_GITHUB_TOKEN}"
    fi

    # ── resolve version ───────────────────────────────────────────
    if [ "$VERSION" = "latest" ]; then
        echo "→ resolving latest release of $REPO..."
        api_url="https://api.github.com/repos/${REPO}/releases/latest"
        # `tag_name` is the first key in the JSON response shape — grep
        # the first occurrence to avoid pulling in jq just for one field.
        if [ -n "$auth_header" ]; then
            # shellcheck disable=SC2086
            latest_json="$(curl $curl_args -H "$auth_header" "$api_url")" || latest_json=""
        else
            # shellcheck disable=SC2086
            latest_json="$(curl $curl_args "$api_url")" || latest_json=""
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

    # ── short-circuit if already installed at the requested version ──
    existing="${INSTALL_DIR}/mxnode"
    if [ "$FORCE" != "1" ] && [ -x "$existing" ]; then
        # `mxnode --version` prints `mxnode <semver>` — the version
        # line itself is whatever the binary's clap derives from
        # Cargo.toml, without the leading `v`. Compare both with and
        # without `v` so a tag of `v0.8.4` matches `0.8.4`.
        current_raw="$("$existing" --version 2>/dev/null | awk 'NR==1 {print $2}' || true)"
        requested_strip="${VERSION#v}"
        if [ -n "$current_raw" ] && [ "$current_raw" = "$requested_strip" ]; then
            echo "✓ mxnode ${VERSION} is already installed at ${existing}"
            echo "  pass --force to reinstall."
            exit 0
        fi
    fi

    # ── pick artefact variant ──────────────────────────────────────
    case "$VARIANT" in
        ""|min) ;;
        *)
            echo "unknown MXNODE_VARIANT='$VARIANT' (valid: empty, min)" >&2
            exit 1
            ;;
    esac
    if [ -n "$VARIANT" ]; then
        archive="mxnode-${VERSION}-${target}-${VARIANT}.tar.gz"
    else
        archive="mxnode-${VERSION}-${target}.tar.gz"
    fi
    archive_url="https://github.com/${REPO}/releases/download/${VERSION}/${archive}"
    sums_url="https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS"
    sig_url="${archive_url}.sig"
    cert_url="${archive_url}.pem"
    release_url="https://github.com/${REPO}/releases/tag/${VERSION}"

    echo "→ installing mxnode ${VERSION} for ${target}${VARIANT:+ (${VARIANT} variant)}"
    echo "  source:  ${release_url}"
    echo "  archive: ${archive_url}"

    # ── download ──────────────────────────────────────────────────
    tmp="$(mktemp -d 2>/dev/null || mktemp -d -t mxnode)"
    trap 'rm -rf "$tmp"' EXIT INT TERM

    # shellcheck disable=SC2086
    curl $curl_args "$archive_url" -o "${tmp}/${archive}" || {
        echo "failed to download $archive_url" >&2
        echo "verify the release exists at https://github.com/${REPO}/releases" >&2
        exit 1
    }
    # shellcheck disable=SC2086
    curl $curl_args "$sums_url" -o "${tmp}/SHA256SUMS" || {
        echo "failed to download SHA256SUMS" >&2
        exit 1
    }

    # ── sha256 ────────────────────────────────────────────────────
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

    # ── cosign (best-effort; opt-in strict via MXNODE_REQUIRE_COSIGN=1) ──
    # Releases starting v0.8.19 are signed via Sigstore keyless cosign
    # using the GitHub Actions OIDC token from the release.yml workflow.
    # Verification matches against:
    #
    #   identity     = https://github.com/<REPO>/.github/workflows/release.yml@refs/tags/<TAG>
    #   oidc issuer  = https://token.actions.githubusercontent.com
    #
    # The .sig and .pem are uploaded alongside the .tar.gz at release
    # time. If they don't exist (older release) OR cosign isn't
    # installed locally, we fall through to sha256-only with an
    # informational note. Setting MXNODE_REQUIRE_COSIGN=1 turns either
    # condition into a hard failure — recommended for production hosts.
    cosign_state=skip
    cosign_reason=""
    if curl -fsSI --retry 3 --retry-delay 2 "$sig_url" >/dev/null 2>&1 \
       && curl -fsSI --retry 3 --retry-delay 2 "$cert_url" >/dev/null 2>&1; then
        if command -v cosign >/dev/null 2>&1; then
            # shellcheck disable=SC2086
            curl $curl_args "$sig_url" -o "${tmp}/${archive}.sig" || cosign_state=fetch_failed
            # shellcheck disable=SC2086
            curl $curl_args "$cert_url" -o "${tmp}/${archive}.pem" || cosign_state=fetch_failed
            if [ "$cosign_state" != "fetch_failed" ]; then
                expected_identity="https://github.com/${REPO}/.github/workflows/release.yml@refs/tags/${VERSION}"
                if cosign verify-blob \
                        --signature "${tmp}/${archive}.sig" \
                        --certificate "${tmp}/${archive}.pem" \
                        --certificate-identity "$expected_identity" \
                        --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
                        "${tmp}/${archive}" >/dev/null 2>&1; then
                    cosign_state=verified
                else
                    cosign_state=mismatch
                fi
            fi
        else
            cosign_state=no_tool
            cosign_reason="cosign not installed (https://docs.sigstore.dev/cosign/installation)"
        fi
    else
        cosign_state=no_sig
        cosign_reason="release does not ship cosign signatures (likely pre-v0.8.19)"
    fi

    case "$cosign_state" in
        verified)
            echo "→ cosign verified (keyless via Sigstore)"
            ;;
        mismatch)
            echo "✗ cosign verification FAILED — refusing to install" >&2
            echo "  signature did not match expected identity:" >&2
            echo "    https://github.com/${REPO}/.github/workflows/release.yml@refs/tags/${VERSION}" >&2
            exit 1
            ;;
        fetch_failed)
            echo "✗ cosign signatures appeared to exist but failed to download" >&2
            exit 1
            ;;
        no_tool|no_sig|skip)
            if [ "$REQUIRE_COSIGN" = "1" ]; then
                echo "✗ MXNODE_REQUIRE_COSIGN=1 set but $cosign_reason" >&2
                exit 1
            fi
            if [ -n "$cosign_reason" ]; then
                echo "  · cosign verification skipped — $cosign_reason"
            fi
            ;;
    esac

    # ── extract ───────────────────────────────────────────────────
    echo "→ extracting..."
    tar -xzf "${tmp}/${archive}" -C "${tmp}"
    [ -x "${tmp}/mxnode" ] || {
        echo "expected mxnode binary missing from archive" >&2
        exit 1
    }

    # ── install ───────────────────────────────────────────────────
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

    # ── post-install verify ───────────────────────────────────────
    # Run the freshly-installed binary and confirm it prints the
    # version we asked for. Catches a corrupted archive that somehow
    # passed sha256 (extremely unlikely), an extraction race, or a
    # stale binary on PATH shadowing the new one.
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
    if [ "$cosign_state" = "verified" ]; then
        echo "  cosign:  verified (keyless via Sigstore)"
    fi
    echo
    "${INSTALL_DIR}/mxnode" --version

    # Friendly tail. mxnode auto-initialises its config (auto-detected
    # $USER/$HOME, network=mainnet) on first state-changing command,
    # so no separate init step is needed.
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
}

# Keep this line at the very bottom of the file. If a `curl | sh` pipe
# is truncated mid-stream, the function above is loaded but never
# invoked, so partial commands cannot execute.
__mxnode_install_main "$@"
