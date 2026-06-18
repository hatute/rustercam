#!/usr/bin/env bash
set -euo pipefail

version="${1:-}"

if [[ -z "$version" ]]; then
  echo "usage: scripts/package-release.sh v0.1.0" >&2
  exit 1
fi

case "$(uname -m)" in
  arm64|aarch64) target="aarch64-apple-darwin" ;;
  x86_64) target="x86_64-apple-darwin" ;;
  *)
    echo "unsupported macOS architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

package="rustercam-${version}-${target}"
out_dir="dist/${package}"
archive="dist/${package}.tar.gz"

cargo build --release

rm -rf "$out_dir" "$archive"
mkdir -p "$out_dir"
cp target/release/rustercam "$out_dir/"
cp README.md LICENSE "$out_dir/"

tar -czf "$archive" -C dist "$package"
shasum -a 256 "$archive" > "${archive}.sha256"

echo "created:"
echo "  $archive"
echo "  ${archive}.sha256"
