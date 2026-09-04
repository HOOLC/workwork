#!/usr/bin/env bash
# Release build with the kache compile cache.
# Runs inside the rust-build container (or any Linux host with cargo):
# pulls remote artifacts, builds release binaries, installs them to
# .data/bin atomically, then pushes new cache entries back to R2.
# All kache steps degrade gracefully when no remote is configured.
set -euo pipefail

target_bins=(zork zork-gateway zork-agent zork-call zork-gh)
data_bin=/src/.data/bin

kache_setup() {
  command -v kache >/dev/null 2>&1 && return 0
  local arch tarball
  case "$(uname -m)" in
    aarch64 | arm64) arch=aarch64 ;;
    x86_64 | amd64) arch=x86_64 ;;
    *) echo "kache: unsupported arch $(uname -m), skipping" >&2; return 1 ;;
  esac
  tarball="kache-${arch}-unknown-linux-musl.tar.gz"
  curl -fsSL --retry 2 \
    "https://github.com/kunobi-ninja/kache/releases/download/v0.16.0/${tarball}" \
    -o /tmp/kache.tar.gz \
    || { echo "kache download failed, continuing without cache" >&2; return 1; }
  tar -xzf /tmp/kache.tar.gz -C /usr/local/bin kache
  chmod +x /usr/local/bin/kache
}

kache_has_remote() {
  [ -n "${KACHE_S3_ACCESS_KEY:-}" ] && [ -n "${KACHE_S3_SECRET_KEY:-}" ]
}

if kache_setup; then
  export RUSTC_WRAPPER=kache
  if kache_has_remote; then
    kache sync --pull --workspace --allow-partial || echo "kache pull failed, building with local cache only" >&2
  else
    echo "KACHE_S3_* not set, skipping remote pull" >&2
  fi
else
  echo "building without kache" >&2
fi

cargo build --release -p zork -p zork-gateway -p zork-agent -p zork-call

mkdir -p "$data_bin"
for bin in "${target_bins[@]}"; do
  cp "/src/target/release/${bin}" "${data_bin}/${bin}.new"
  chmod +x "${data_bin}/${bin}.new"
  mv -f "${data_bin}/${bin}.new" "${data_bin}/${bin}"
  echo "wrote ${data_bin}/${bin}"
done

if command -v kache >/dev/null 2>&1 && kache_has_remote; then
  kache sync --push --allow-partial || echo "kache push failed" >&2
fi
