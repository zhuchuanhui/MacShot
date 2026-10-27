#!/bin/sh
set -eu

app_path=${1:-output/MacShot.app}
info_plist="$app_path/Contents/Info.plist"

if [ ! -d "$app_path" ] || [ ! -f "$info_plist" ]; then
    printf 'macOS app bundle not found: %s\n' "$app_path" >&2
    exit 1
fi

for key in NSMicrophoneUsageDescription NSScreenCaptureUsageDescription; do
    value=$(plutil -extract "$key" raw -o - "$info_plist" 2>/dev/null || true)
    if [ -z "$value" ]; then
        printf 'Required privacy description is missing or empty: %s\n' "$key" >&2
        exit 1
    fi
done

codesign --verify --deep --strict "$app_path"
if ! codesign -d --entitlements :- "$app_path" 2>/dev/null \
    | plutil -convert json -o - - \
    | tr -d ' ' \
    | grep -qF '"com.apple.security.device.audio-input":true'; then
    printf 'Required code-signing entitlement is missing or false: com.apple.security.device.audio-input\n' >&2
    exit 1
fi

printf 'Verified privacy descriptions and code signature: %s\n' "$app_path"
