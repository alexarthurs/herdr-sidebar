#!/bin/sh
set -eu

root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
fixture="$root/fixture"
out="$root/out/herdr-sidebar"
mkdir -p "$fixture"
version=$(grep -E '^version *= *"' Cargo.toml | head -n1 | sed -E 's/^version *= *"([^"]+)".*/\1/')
test_os=${HS_UNAME_S:-$(uname -s)}
test_arch=${HS_UNAME_M:-$(uname -m)}
case "$test_os/$test_arch" in
  Linux/x86_64) triple=x86_64-unknown-linux-musl ;;
  Linux/aarch64|Linux/arm64) triple=aarch64-unknown-linux-musl ;;
  Darwin/arm64) triple=aarch64-apple-darwin ;;
  Darwin/x86_64) triple=x86_64-apple-darwin ;;
  *) echo "unsupported test runner" >&2; exit 1 ;;
esac
asset="herdr-sidebar-$triple"
printf 'verified-prebuilt-%s\n' "$version" > "$fixture/$asset"
arm_asset="herdr-sidebar-aarch64-unknown-linux-musl"
printf 'verified-arm64-prebuilt-%s\n' "$version" > "$fixture/$arm_asset"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$fixture" && sha256sum "$asset" "$arm_asset" > SHA256SUMS)
else
  (cd "$fixture" && shasum -a 256 "$asset" "$arm_asset" > SHA256SUMS)
fi

HS_TEST_MODE=1 HS_FETCH_DIR="$fixture" HS_OUT="$out" HS_CARGO_TOML="$PWD/Cargo.toml" \
  sh scripts/fetch-or-build.sh
cmp "$fixture/$asset" "$out"

arm_out="$root/out/herdr-sidebar-arm64"
HS_TEST_MODE=1 HS_UNAME_S=Linux HS_UNAME_M=aarch64 HS_FETCH_DIR="$fixture" \
  HS_OUT="$arm_out" HS_CARGO_TOML="$PWD/Cargo.toml" sh scripts/fetch-or-build.sh
cmp "$fixture/$arm_asset" "$arm_out"

rm -f "$fixture/$asset"
marker="$root/fallback"
stub="$root/cargo"
cat > "$stub" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" > "$HS_FALLBACK_MARKER"
EOF
chmod +x "$stub"
HS_TEST_MODE=1 HS_FETCH_DIR="$fixture" HS_OUT="$out" HS_CARGO_TOML="$PWD/Cargo.toml" \
  HS_CARGO="$stub" HS_FALLBACK_MARKER="$marker" sh scripts/fetch-or-build.sh
grep -Fx 'build --release' "$marker"

printf 'tampered\n' > "$fixture/$asset"
rm -f "$marker"
HS_TEST_MODE=1 HS_FETCH_DIR="$fixture" HS_OUT="$out" HS_CARGO_TOML="$PWD/Cargo.toml" \
  HS_CARGO="$stub" HS_FALLBACK_MARKER="$marker" sh scripts/fetch-or-build.sh
grep -Fx 'build --release' "$marker"
