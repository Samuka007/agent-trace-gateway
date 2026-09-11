#!/usr/bin/env bash
# egress probe from atcd-dev — ticket 01 step 3
echo "=== domain reachability (direct) ==="
for d in github.com codeload.github.com cache.nixos.org static.rust-lang.org index.crates.io static.crates.io channels.nixos.org; do
  out=$(curl -sS -o /dev/null -w '%{http_code} %{time_total}s' --connect-timeout 8 --max-time 20 "https://$d" 2>&1)
  echo "$d -> $out"
done

echo "=== artifact download probes (direct) ==="
echo -n "static.crates.io serde-1.0.210.crate 1MiB: "
curl -sS -o /dev/null -w '%{http_code} speed=%{speed_download}B/s\n' --max-time 40 -r 0-1048576 "https://static.crates.io/crates/serde/serde-1.0.210.crate" 2>&1
echo -n "cache.nixos.org narinfo (xxhash-0.8.3): "
curl -sS -o /dev/null -w '%{http_code} %{time_total}s\n' --max-time 20 "https://cache.nixos.org/0ll8j4gj5x2zs020dhhi02fhv3f5whvl.narinfo" 2>&1
echo -n "static.rust-lang.org channel-rust-1.95.0.toml: "
curl -sS -o /dev/null -w '%{http_code} %{time_total}s\n' --max-time 20 "https://static.rust-lang.org/dist/channel-rust-1.95.0.toml" 2>&1
echo -n "codeload.github.com nixpkgs-unstable tar.gz 1MiB: "
curl -sSL -o /dev/null -w '%{http_code} speed=%{speed_download}B/s\n' --max-time 60 "https://codeload.github.com/NixOS/nixpkgs/tar.gz/refs/heads/nixpkgs-unstable" 2>&1
echo -n "github.com Samuka007/codex atcd-libs tar.gz 1MiB: "
curl -sSL -o /dev/null -w '%{http_code} speed=%{speed_download}B/s\n' --max-time 60 "https://codeload.github.com/Samuka007/codex/tar.gz/9f95fe18" 2>&1

echo "=== proxy fallback probe (socks5 10.0.100.191:10808) ==="
out=$(curl -sS --socks5-hostname 10.0.100.191:10808 -o /dev/null -w '%{http_code} %{time_total}s' --connect-timeout 8 --max-time 20 "https://github.com" 2>&1)
echo "github.com via proxy -> $out"
out=$(curl -sS --socks5-hostname 10.0.100.191:10808 -o /dev/null -w '%{http_code} %{time_total}s' --connect-timeout 8 --max-time 20 "https://static.rust-lang.org" 2>&1)
echo "static.rust-lang.org via proxy -> $out"
