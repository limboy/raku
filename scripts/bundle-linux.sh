#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target_dir="${CARGO_TARGET_DIR:-target}"
version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"raku","version":"\([^"]*\)".*/\1/p')"
target_triple="$(rustc -vV | sed -n 's/^host: //p')"
package="raku-${version}-${target_triple}"
archive="$target_dir/release/$package.tar.gz"
staging="$(mktemp -d)"
trap 'rm -rf -- "$staging"' EXIT

cargo build --locked --release \
  --package raku --bin raku --bin raku-updater \
  --package raku-daemon --bin raku-daemon

package_dir="$staging/$package"
install -Dm755 "$target_dir/release/raku" "$package_dir/bin/raku"
install -Dm755 "$target_dir/release/raku-updater" "$package_dir/bin/raku-updater"
install -Dm755 "$target_dir/release/raku-daemon" "$package_dir/bin/raku-daemon"
install -Dm644 resources/linux/sh.raku.desktop \
  "$package_dir/share/applications/sh.raku.desktop"
install -Dm644 resources/linux/self-update-v1 \
  "$package_dir/share/raku/self-update-v1"
for pi_extension in resources/pi/*.ts; do
  install -Dm644 "$pi_extension" \
    "$package_dir/share/raku/pi/$(basename "$pi_extension")"
done
install -Dm644 website/public/app-icon.png \
  "$package_dir/share/icons/hicolor/256x256/apps/sh.raku.png"
install -Dm644 LICENSE "$package_dir/share/licenses/raku/LICENSE"

mkdir -p "$(dirname "$archive")"
tar -C "$staging" -czf "$archive" "$package"
printf 'Created %s\n' "$archive"
