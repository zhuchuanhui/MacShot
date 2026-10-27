#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "Usage: $0 <version> <windows-x64-executable>" >&2
  exit 2
fi

version="$1"
executable="$2"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][A-Za-z0-9.-]+)?$ ]]; then
  echo "Invalid release version: $version" >&2
  exit 2
fi
if [[ ! -f "$executable" ]]; then
  echo "Windows executable not found: $executable" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
release_root="$repo_root/output/release"
package_name="MacShot-v${version}-windows-x64"
package_dir="$release_root/$package_name"
archive="$release_root/$package_name.zip"
checksum="$archive.sha256"
mingw_cxx="${MINGW_CXX:-x86_64-w64-mingw32-g++}"
mingw_objdump="${MINGW_OBJDUMP:-x86_64-w64-mingw32-objdump}"

for tool in "$mingw_cxx" "$mingw_objdump" zip unzip shasum; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Required packaging tool not found: $tool" >&2
    exit 1
  fi
done

if [[ -e "$package_dir" || -e "$archive" || -e "$checksum" ]]; then
  echo "Refusing to overwrite an existing package for version $version" >&2
  exit 1
fi

sysroot="$("$mingw_cxx" -print-sysroot)"
if [[ -z "$sysroot" || ! -d "$sysroot" ]]; then
  echo "Could not locate the MinGW sysroot from $mingw_cxx" >&2
  exit 1
fi

resolve_runtime_dll() {
  local name="$1"
  local candidate
  candidate="$("$mingw_cxx" -print-file-name="$name")"
  if [[ -f "$candidate" ]]; then
    printf '%s\n' "$candidate"
    return 0
  fi
  for candidate in \
    "$sysroot/x86_64-w64-mingw32/bin/$name" \
    "$sysroot/x86_64-w64-mingw32/lib/$name"; do
    if [[ -f "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  echo "Required MinGW runtime DLL not found: $name" >&2
  return 1
}

is_windows_system_dll() {
  local name
  name="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  case "$name" in
    api-ms-win-*.dll|ext-ms-win-*.dll|advapi32.dll|bcryptprimitives.dll|comctl32.dll|d3d11.dll|dwmapi.dll|gdi32.dll|imm32.dll|kernel32.dll|mfplat.dll|mmdevapi.dll|msvcrt.dll|ntdll.dll|ole32.dll|oleaut32.dll|opengl32.dll|psapi.dll|shell32.dll|shlwapi.dll|uiautomationcore.dll|user32.dll|uxtheme.dll|version.dll)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

mkdir -p "$release_root"
mkdir "$package_dir"
cp "$executable" "$package_dir/MacShot.exe"
cp "$repo_root/docs/windows/README.txt" "$package_dir/README.txt"
cp "$repo_root/docs/windows/THIRD-PARTY-NOTICES.txt" "$package_dir/THIRD-PARTY-NOTICES.txt"
cp "$repo_root/docs/windows/COPYING.RUNTIME" "$package_dir/COPYING.RUNTIME"
cp "$repo_root/docs/windows/COPYING3" "$package_dir/COPYING3"
cp "$repo_root/docs/windows/WINPTHREADS-COPYING" "$package_dir/WINPTHREADS-COPYING"

pending=("$package_dir/MacShot.exe")
while [[ ${#pending[@]} -gt 0 ]]; do
  current="${pending[0]}"
  pending=("${pending[@]:1}")
  while IFS= read -r dependency; do
    [[ -z "$dependency" ]] && continue
    if is_windows_system_dll "$dependency"; then
      continue
    fi
    dependency_lower="$(printf '%s' "$dependency" | tr '[:upper:]' '[:lower:]')"
    if [[ "$dependency_lower" != lib*.dll ]]; then
      echo "Unclassified Windows dependency in $(basename "$current"): $dependency" >&2
      exit 1
    fi
    destination="$package_dir/$dependency"
    if [[ ! -f "$destination" ]]; then
      source="$(resolve_runtime_dll "$dependency")"
      cp "$source" "$destination"
      pending+=("$destination")
    fi
  done < <("$mingw_objdump" -p "$current" | awk '/DLL Name:/{print $3}')
done

(
  cd "$release_root"
  zip -X -q -r "$package_name.zip" "$package_name"
  shasum -a 256 "$package_name.zip" > "$package_name.zip.sha256"
  unzip -tq "$package_name.zip"
  shasum -a 256 -c "$package_name.zip.sha256"
)

echo "Created $archive"
