#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    printf 'Usage: %s <MacShot.app> <codesign identity>\n' "$0" >&2
    exit 2
fi

app_path=$1
identity=$2
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
entitlements="$script_dir/../assets/MacShot.entitlements"

if [ ! -d "$app_path" ]; then
    printf 'macOS app bundle not found: %s\n' "$app_path" >&2
    exit 1
fi

if [ ! -f "$entitlements" ]; then
    printf 'macOS entitlements file not found: %s\n' "$entitlements" >&2
    exit 1
fi

codesign --force --deep --options runtime --timestamp --entitlements "$entitlements" --sign "$identity" "$app_path"
"$script_dir/verify_macos_bundle.sh" "$app_path"
