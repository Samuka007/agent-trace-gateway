# atcd-dev — build/test executor container on dragonos-1288v3 (Incus).
# Ticket: .scratch/atcd-build-infra/issues/01-provision-atcd-dev-container.md
# Base structure kept from the images:nixos/26.05 stock configuration.nix
# (systemd-networkd DHCP on eth0; the fixed 10.0.100.244 comes from the
# Incus nic device reservation on the host side).
{ modulesPath, pkgs, ... }:
{
  imports = [
    "${modulesPath}/virtualisation/lxc-container.nix"
    ./incus.nix
  ];

  time.timeZone = "Asia/Shanghai";

  networking = {
    dhcpcd.enable = false;
    useDHCP = false;
    useHostResolvConf = false;
  };
  systemd.network = {
    enable = true;
    networks."50-eth0" = {
      matchConfig.Name = "eth0";
      networkConfig = {
        DHCP = "ipv4";
        IPv6AcceptRA = true;
      };
      linkConfig.RequiredForOnline = "routable";
    };
  };

  # ── egress: split proxy, mirror-first (China network, ticket 01 step 3;
  # mihomo egress change approved by user via Main steer 2026-09-11) ──
  # Measured 2026-09-11 from this container:
  #   - github.com/<owner>/<repo>/archive/*.tar.gz (nix flake fetch path):
  #     DIRECT BLOCKED (0 bytes in 45s timeout) → must go via proxy.
  #   - mihomo 10.0.100.240 (PRIMARY): socks5h :10809 github 200 in 0.83s,
  #     throughput 3.18 MB/s on a real 53MB nixpkgs tarball; http :10808
  #     200 in 0.84s, 2.12 MB/s. Whitelist gate already contains
  #     10.0.100.244/32 (mihomo.nix + config.yaml lan-allowed-ips).
  #   - xray 10.0.100.191:10808 (VERIFIED FALLBACK): github 200 in 2.6s,
  #     archive URL 200 in 3.3s; slower than mihomo.
  #   - codeload.github.com: direct works (1.1–3.9 MB/s) but same GFW
  #     family as github.com → deliberately proxied for robustness
  #     (measured 3.18 MB/s through mihomo; no noProxy entry).
  #   - cache.nixos.org / static.rust-lang.org / index.crates.io /
  #     static.crates.io: direct works (narinfo 0.67s; crate dl 206 but
  #     ~78KB/s → cargo uses rsproxy mirror instead, see activationScript).
  # socks5h (remote DNS at the proxy) avoids GFW DNS pollution.
  networking.proxy = {
    default = "socks5h://10.0.100.240:10809";
    noProxy = "127.0.0.1,localhost,10.0.100.0/24,cache.nixos.org,channels.nixos.org,static.rust-lang.org,index.crates.io,static.crates.io,mirrors.tuna.tsinghua.edu.cn,rsproxy.cn";
  };

  # ── atcd build/test executor requirements ──────────────────────────
  nix = {
    settings = {
      experimental-features = [ "nix-command" "flakes" ];
      trusted-users = [ "root" "samuka" ];
    };
  };

  environment.systemPackages = with pkgs; [
    git
    rsync
    curl
    vim
    htop
  ];

  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = false;
      PermitRootLogin = "prohibit-password";
    };
  };

  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPpjclIQCbrsS/MhKyx5/m9ZGUNfkOMmxunxBidFQBnn nixos@nixos"
  ];

  users.users.samuka = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
    openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPpjclIQCbrsS/MhKyx5/m9ZGUNfkOMmxunxBidFQBnn nixos@nixos"
    ];
  };
  security.sudo.wheelNeedsPassword = false;

  # Mirror-first for cargo (user ruling): crates.io → rsproxy-sparse.
  # Declared here so /root and /home/samuka get the same config on every
  # switch; imperative ~/.cargo edits will be overwritten (by design).
  system.activationScripts.rsproxy = let
    cargoConfig = pkgs.writeText "cargo-config.toml" ''
      [source.crates-io]
      replace-with = 'rsproxy-sparse'
      [source.rsproxy-sparse]
      registry = "sparse+https://rsproxy.cn/index/"
      [registries.rsproxy]
      index = "https://rsproxy.cn/crates.io-index"
      [net]
      git-fetch-with-cli = true
    '';
  in ''
    install -Dm644 ${cargoConfig} /root/.cargo/config.toml
    install -Dm644 ${cargoConfig} /home/samuka/.cargo/config.toml
    chown samuka:users /home/samuka/.cargo/config.toml
  '';

  system.stateVersion = "26.05";
}
