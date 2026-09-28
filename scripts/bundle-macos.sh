#!/usr/bin/env bash
# Build Joust.app for macOS and put it on a disk image, signed and notarised
# when credentials are given:
#
#   dist/joust-<version>-<target>.dmg          Joust.app and an Applications link
#   dist/joust-<version>-<target>.dmg.sha256   its SHA-256 checksum
#
# Usage: scripts/bundle-macos.sh [target]   (default: this Mac's target)
#
# Signing: MACOS_SIGN_IDENTITY names a "Developer ID Application" identity in
# the keychain (its name or SHA-1 hash). Without it the app gets an ad-hoc
# signature, which runs locally but which Gatekeeper rejects in a download.
#
# Notarisation (needs MACOS_SIGN_IDENTITY; set all three or none): NOTARY_KEY is
# the path of an App Store Connect API key (AuthKey_<id>.p8), NOTARY_KEY_ID its
# key ID and NOTARY_ISSUER its issuer ID. The notarisation ticket is stapled to
# the disk image, so it also verifies offline.
set -euo pipefail

cd "$(dirname "$0")/.."

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
if [[ "$target" != *-apple-darwin ]]; then
    echo "error: ${target} is not a macOS target" >&2
    exit 1
fi
# `cargo pkgid` prints `path+file:///…/joust#0.1.0` (or `…#joust@0.1.0`).
version="$(cargo pkgid | sed 's/.*[#@]//')"
name="joust-${version}-${target}"
identity="${MACOS_SIGN_IDENTITY:-}"

notarise=false
if [[ -n "${NOTARY_KEY:-}${NOTARY_KEY_ID:-}${NOTARY_ISSUER:-}" ]]; then
    if [[ -z "${NOTARY_KEY:-}" || -z "${NOTARY_KEY_ID:-}" || -z "${NOTARY_ISSUER:-}" ]]; then
        echo "error: set all of NOTARY_KEY, NOTARY_KEY_ID and NOTARY_ISSUER, or none" >&2
        exit 1
    fi
    if [[ -z "$identity" ]]; then
        echo "error: notarisation needs MACOS_SIGN_IDENTITY" >&2
        exit 1
    fi
    notarise=true
fi

cargo build --release --locked --target "$target"

# The staging directory becomes the disk image's contents.
stage="target/dist/${name}"
app="${stage}/Joust.app"
rm -rf "$stage"
mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Resources" dist
cp "target/${target}/release/joust" "${app}/Contents/MacOS/joust"
# Bundle versions must be numeric, so a pre-release suffix is dropped
# (0.2.0-rc.1 → 0.2.0); the disk image's name keeps it.
sed "s/@VERSION@/${version%%[-+]*}/g" macos/Info.plist >"${app}/Contents/Info.plist"
plutil -lint "${app}/Contents/Info.plist"

# Every icon size macOS uses, scaled from the 1024px render.
iconset="target/dist/joust.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/joust-icon.png \
        --out "${iconset}/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) assets/joust-icon.png \
        --out "${iconset}/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "${app}/Contents/Resources/joust.icns"

# Notarisation requires the hardened runtime and a secure timestamp. The ad-hoc
# signature uses the hardened runtime too, so unsigned builds behave the same.
if [[ -n "$identity" ]]; then
    codesign --force --sign "$identity" --options runtime --timestamp "$app"
else
    codesign --force --sign - --options runtime "$app"
fi
codesign --verify --strict --verbose=2 "$app"
codesign --display --verbose=2 "$app"

ln -s /Applications "${stage}/Applications"
dmg="dist/${name}.dmg"
rm -f "$dmg"
# hdiutil occasionally fails with "Resource busy" while another process (such
# as a malware scan) still has the staged files open; a retry gets past it.
for attempt in 1 2 3; do
    if hdiutil create -volname "Joust ${version}" -srcfolder "$stage" -format UDZO -ov "$dmg"; then
        break
    fi
    if ((attempt == 3)); then
        exit 1
    fi
    sleep 10
done

if [[ -n "$identity" ]]; then
    codesign --force --sign "$identity" --timestamp "$dmg"
fi

if [[ "$notarise" == true ]]; then
    auth=(--key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER")
    result="${stage}.notary.json"
    # Notarising the disk image covers the app inside it.
    xcrun notarytool submit "$dmg" "${auth[@]}" --wait --timeout 1h \
        --output-format json >"$result" || true
    status="$(plutil -extract status raw -o - "$result" 2>/dev/null || echo unknown)"
    if [[ "$status" != "Accepted" ]]; then
        cat "$result" >&2
        if id="$(plutil -extract id raw -o - "$result" 2>/dev/null)"; then
            xcrun notarytool log "$id" "${auth[@]}" >&2 || true
        fi
        echo "error: notarisation failed (status: ${status})" >&2
        exit 1
    fi
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
    # What Gatekeeper checks when a downloaded disk image is opened.
    spctl --assess --type open --context context:primary-signature --verbose=2 "$dmg"
fi

(cd dist && shasum -a 256 "${name}.dmg" >"${name}.dmg.sha256")

echo "$dmg"
