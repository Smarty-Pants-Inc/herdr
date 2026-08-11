#!/bin/sh
set -eu

BIN=herdr
MANIFEST_URL="${HERDR_MANIFEST_URL:-https://raw.githubusercontent.com/Smarty-Pants-Inc/herdr/smarty-channel/preview.json}"
INSTALL_DIR="${HERDR_INSTALL_DIR:-$HOME/.local/bin}"
PLUTIL="${HERDR_PLUTIL:-/usr/bin/plutil}"


log() { printf '%s\n' "herdr: $1"; }
warn() { printf '%s\n' "herdr: warning: $1" >&2; }
err() { printf '%s\n' "herdr: $1" >&2; exit 1; }

need() {
    command -v "$1" >/dev/null 2>&1 || err "requires $1"
}

manifest_value() {
    printf '%s\n' "$MANIFEST" | "$PLUTIL" -extract "$1" raw -o - - 2>/dev/null
}

sha256() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{ print $NF }'
    else
        err "SHA-256 verification requires shasum, sha256sum, or openssl"
    fi
}


main() {
    [ "$(uname -s)" = Darwin ] || err "Smarty preview installation supports macOS only"
    case "$INSTALL_DIR" in
        /*) ;;
        *) err "HERDR_INSTALL_DIR must be an absolute path" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64) TARGET=macos-x86_64 ;;
        arm64|aarch64) TARGET=macos-aarch64 ;;
        *) err "unsupported architecture: $(uname -m)" ;;
    esac

    need awk
    need curl
    need mktemp
    [ -x "$PLUTIL" ] || err "requires $PLUTIL"

    log "fetching preview manifest"
    MANIFEST="$(curl -fsSL --retry 3 --connect-timeout 10 --max-time 20 "$MANIFEST_URL")" \
        || err "cannot fetch $MANIFEST_URL"
    CHANNEL="$(manifest_value channel || true)"
    [ "$CHANNEL" = preview ] || err "update manifest is not the Smarty preview channel"
    URL="$(manifest_value "assets.$TARGET.url" || true)"
    EXPECTED_SHA256="$(manifest_value "assets.$TARGET.sha256" || true)"
    [ -n "$URL" ] || err "preview manifest does not include $TARGET"
    case "$EXPECTED_SHA256" in
        *[!0123456789abcdefABCDEF]*|'') err "preview manifest does not include a valid SHA-256 checksum for $TARGET" ;;
    esac
    [ "${#EXPECTED_SHA256}" -eq 64 ] || err "preview manifest does not include a valid SHA-256 checksum for $TARGET"
    EXPECTED_SHA256="$(printf '%s\n' "$EXPECTED_SHA256" | awk '{ print tolower($0) }')"

    tmp="$(mktemp -d "${TMPDIR:-/tmp}/herdr.XXXXXX")"
    trap 'rm -rf "$tmp"' EXIT HUP INT TERM
    curl -fsSL --retry 3 --connect-timeout 10 --max-time 120 "$URL" -o "$tmp/$BIN" \
        || err "download failed from $URL"
    actual_sha256="$(sha256 "$tmp/$BIN")"
    [ "$actual_sha256" = "$EXPECTED_SHA256" ] || err "downloaded Herdr checksum did not match"

    mkdir -p "$INSTALL_DIR"
    installed="$INSTALL_DIR/$BIN"
    install_tmp="$(mktemp "$INSTALL_DIR/.${BIN}.XXXXXX")"
    chmod 755 "$tmp/$BIN"
    mv "$tmp/$BIN" "$install_tmp"
    mv "$install_tmp" "$installed"
    [ -f "$installed" ] && [ ! -L "$installed" ] || err "installed launcher is not a regular file"

    resolved="$(command -v "$BIN" 2>/dev/null || true)"
    if [ "$resolved" != "$installed" ]; then
        warn "'$BIN' resolves to ${resolved:-nothing}, not $installed; add $INSTALL_DIR before other Herdr paths in PATH"
    fi
    log "installed self-updating client at $installed"
}

main "$@"
