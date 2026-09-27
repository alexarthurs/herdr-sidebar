#!/bin/sh
# Install a release binary when one matches this checkout's declared version.
# Any unsupported platform, download failure, or checksum mismatch falls back
# to the historical source build instead of making installation less reliable.
set -u

repo="alexarthurs/herdr-sidebar"
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
test_mode=${HS_TEST_MODE:-0}
if [ "$test_mode" = 1 ]; then
  repo_root=${HS_REPO_ROOT:-"$script_dir/.."}
  cargo_toml=${HS_CARGO_TOML:-"$repo_root/Cargo.toml"}
  out=${HS_OUT:-"$repo_root/target/release/herdr-sidebar"}
  base_url=${HS_BASE_URL:-"https://github.com/$repo/releases/download"}
else
  repo_root="$script_dir/.."
  cargo_toml="$repo_root/Cargo.toml"
  out="$repo_root/target/release/herdr-sidebar"
  base_url="https://github.com/$repo/releases/download"
fi

cleanup() {
  [ -n "${tmpdir:-}" ] && rm -rf "$tmpdir"
}

build_from_source() {
  cleanup
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if [ "$test_mode" = 1 ]; then cargo_bin=${HS_CARGO:-cargo}; else cargo_bin=cargo; fi
  if ! command -v "$cargo_bin" >/dev/null 2>&1; then
    echo "herdr-sidebar needs a matching prebuilt release or Rust from https://rustup.rs" >&2
    exit 1
  fi
  cd "$repo_root" || exit 1
  exec "$cargo_bin" build --release
}

fallback() {
  echo "herdr-sidebar: $1; building from source instead." >&2
  build_from_source
}

download() {
  name=$1
  dest=$2
  if [ "$test_mode" = 1 ] && [ -n "${HS_FETCH_DIR:-}" ]; then
    cp "$HS_FETCH_DIR/$name" "$dest" 2>/dev/null
  elif command -v curl >/dev/null 2>&1; then
    curl -fsSL -o "$dest" "$base_url/v$version/$name"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$dest" "$base_url/v$version/$name"
  else
    return 127
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 127
  fi
}

if [ "$test_mode" = 1 ]; then
  os=${HS_UNAME_S:-$(uname -s 2>/dev/null || echo unknown)}
  arch=${HS_UNAME_M:-$(uname -m 2>/dev/null || echo unknown)}
else
  os=$(uname -s 2>/dev/null || echo unknown)
  arch=$(uname -m 2>/dev/null || echo unknown)
fi
triple=""
case "$os/$arch" in
  Darwin/arm64|Darwin/aarch64) triple="aarch64-apple-darwin" ;;
  Darwin/x86_64|Darwin/amd64) triple="x86_64-apple-darwin" ;;
  Linux/aarch64|Linux/arm64) triple="aarch64-unknown-linux-musl" ;;
  Linux/x86_64|Linux/amd64) triple="x86_64-unknown-linux-musl" ;;
esac
[ -n "$triple" ] || fallback "no prebuilt binary for $os/$arch"

version=$(grep -E '^version *= *"' "$cargo_toml" 2>/dev/null | head -n 1 | sed -E 's/^version *= *"([^"]+)".*/\1/')
[ -n "$version" ] || fallback "could not read the crate version"

asset="herdr-sidebar-$triple"
tmpdir=$(mktemp -d 2>/dev/null) || fallback "could not create a temporary directory"
trap cleanup EXIT
download SHA256SUMS "$tmpdir/SHA256SUMS" || fallback "checksums are unavailable for v$version"
download "$asset" "$tmpdir/$asset" || fallback "no prebuilt $asset exists for v$version"

expected=$(grep -E "^[0-9a-fA-F]{64} [ *]$asset\$" "$tmpdir/SHA256SUMS" 2>/dev/null | awk '{print tolower($1)}' | head -n 1)
[ -n "$expected" ] || fallback "the release does not list a checksum for $asset"
actual=$(sha256_of "$tmpdir/$asset") || fallback "no SHA-256 utility is available"
[ "$actual" = "$expected" ] || fallback "checksum verification failed for $asset"

mkdir -p "$(dirname "$out")" || fallback "could not create the binary directory"
chmod +x "$tmpdir/$asset"
mv -f "$tmpdir/$asset" "$out" || fallback "could not install the verified binary"
echo "herdr-sidebar: installed verified prebuilt v$version ($triple)."
exit 0
