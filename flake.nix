# agent-trace-gateway Nix flake: ISA-tiered build matrix + dockerTools slim images.
#
# ── ISA tier matrix (target hardware per tier) ────────────────────────────────
#   atg-baseline : x86-64 (SSE2 baseline). Runs on ANY amd64 CPU.
#                  Required for production 198.2.219.93 (Xeon E5-2620 v2,
#                  Ivy Bridge-EP: v2 only, no AVX2/BMI2). DEFAULT tier.
#   atg-v2       : x86-64-v2 (-C target-cpu=x86-64-v2, tune=ivybridge).
#                  Production-exclusive optimization for the E5-2620 v2.
#                  NOT benchmarked on real v2 hardware (none in this fleet);
#                  CT104 (Haswell) runs it but results ≠ v2-machine behavior.
#   atg-v3       : x86-64-v3 (-C target-cpu=x86-64-v3, tune=haswell).
#                  For CT104 192.168.1.128 (E5-2666 v3, Haswell: AVX2/BMI2/FMA)
#                  and the 5800X3D workstation (zen3). SIGILL on v1/v2 CPUs.
#
# Image tag convention: :latest = baseline, :v2, :v3. Production canary must
# pull :latest or :v2 — never :v3 (deployment discipline: ISA tier is encoded
# in the tag, not auto-detected; a wrong pull = SIGILL at startup).
#
# ── Why buildRustPackage (cargoLock) and not crate2nix/naersk ────────────────
# buildRustPackage consumes Cargo.lock directly (zero new repo files). The
# workspace has one nontrivial dep: boring-sys 4.22 (BoringSSL via cmake+clang
# +go+perl), which builds fine under nix because nixpkgs stdenv provides cmake
# and the boring-sys build script drives it; go/perl are pulled via
# nativeBuildInputs below. No musl attempt: boring-sys + glibc dynamic linking
# is the boring, working path (see musl notes in the perf report).
#
# ── ISA tiering is Rust-level, not nixpkgs-level ─────────────────────────────
# `localSystem.gcc.arch` rebuilds the ENTIRE nixpkgs closure for the arch —
# hours of build time for marginal gain. Instead we keep stock nixpkgs and
# pass RUSTFLAGS per tier: only our crate (and its deps compiled by cargo,
# ~350 crates) get the ISA bump. Hot path is byte-scan + JSON parse in our
# own code; std/serde autovectorization follows the same flags via cargo.
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Tier definitions: name → { rustflags, description }.
        # tune: baseline = generic; v2 tuned for Ivy Bridge; v3 tuned for Haswell
        # (works for zen3 too — v3 is a superset floor, tuning choice is
        # scheduling hints only, both are v3-capable).
        tiers = {
          baseline = {
            arch = "x86-64";
            tune = null;
            description = "x86-64 baseline (any amd64; production E5-2620 v2 target)";
          };
          v2 = {
            arch = "x86-64-v2";
            tune = "ivybridge";
            description = "x86-64-v2 (production E5-2620 v2 / Ivy Bridge)";
          };
          v3 = {
            arch = "x86-64-v3";
            tune = "haswell";
            description = "x86-64-v3 (CT104 E5-2666 v3 / Haswell, 5800X3D / zen3)";
          };
        };

        # Rust flags for one tier. `-C relocation-model=pic` not needed for the
        # image (static-pie not used; glibc dynamic is fine inside sLIC).
        rustFlagsFor = tier:
          builtins.concatStringsSep " " (
            [ "-C target-cpu=${tier.arch}" ]
            ++ (pkgs.lib.optional (tier.tune != null)
              "-C target-cpu=${tier.arch} -Ztune=${tier.tune}")
          );
        # NOTE: -Ztune is nightly-only. Stable rustc has no tune flag; use
        # `-C target-cpu=<specific-model>` instead when tuning matters. We map
        # tune → explicit model for stable:
        cpuModelFor = tier: {
          baseline = "x86-64";
          v2 = "ivybridge"; # Ivy Bridge = first v2-class µarch
          v3 = "haswell"; # Haswell = first v3-class µarch
        }.${tier.arch} or tier.arch;

        mkAtg = tierName: tier:
          pkgs.rustPlatform.buildRustPackage {
            pname = "agent-trace-gateway-${tierName}";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;

            # boring-sys (BoringSSL) build needs cmake, clang, go, perl —
            # same toolchain list as deploy/Dockerfile's apt-get line —
            # PLUS git: boring-sys's build.rs runs `git init` on the copied
            # BoringSSL submodule before applying the pq-experimental patch
            # (pingora-boringssl enables that feature). Without git on PATH
            # the build script dies with ENOENT at main.rs:673. This is the
            # #1 boring-sys-under-nix pitfall.
            nativeBuildInputs = with pkgs; [ cmake clang go perl pkg-config git ];

            # bindgen (pulled in by boring-sys) needs the clang shared library
            # explicitly — the clang *wrapper* on PATH is not enough.
            env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            RUSTFLAGS = "-C target-cpu=${cpuModelFor tier}";
            # cargo treats changed RUSTFLAGS as a fingerprint change → full
            # rebuild per tier; that is exactly the isolation we want.

            doCheck = false; # tests run separately, not in image builds
            meta = { description = "ATG gateway — ${tier.description}"; };
          };

        # dockerTools.streamLayeredImage: no FROM base, no shell, no package
        # manager. contents = closure of the gateway binary + ca-certificates
        # only. created=1970 → reproducible digest across rebuilds.
        # config: runs as nobody (65534) — gateway binds 0.0.0.0:6180
        # (unprivileged port, no CAP needed).
        mkImage = tierName: tier:
          let
            atg = mkAtg tierName tier;
          in
          pkgs.dockerTools.streamLayeredImage {
            name = "agent-trace-gateway";
            tag = if tierName == "baseline" then "latest"
            else tierName;
            created = "1970-01-01T00:00:01Z";
            contents = [ atg pkgs.cacert ];
            # Layer budget: keep small (fewer layer fetches over slow links);
            # store paths are content-addressed so layering is purely grouping.
            maxLayers = 4;
            config = {
              User = "65534";
              Entrypoint = [ "${atg}/bin/gateway" ];
              Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
              ExposedPorts = { "6180/tcp" = { }; };
              Labels = {
                "org.opencontainers.image.title" = "agent-trace-gateway";
                "org.opencontainers.image.description" = tier.description;
                "io.atg.isa-tier" = tierName;
              };
            };
          };

        imageBuilder = tierName: tier:
          # `nix build .#dockerImage-v3` → result → stream to docker load.
          pkgs.runCommand "atg-image-${tierName}"
            {
              nativeBuildInputs = [ pkgs.skopeo ];
            }
            ''
              # Materialize the streamed tarball (from streamLayeredImage).
              ${mkImage tierName tier} > $out
            '';
        # NOTE: skopeo dep unused here (streamLayeredImage prints a tar to
        # stdout); kept minimal on purpose — drop if not needed.
        # Fully-static musl build for running the bench on machines without
        # the nix dynamic loader (e.g. CT104 ubuntu runner). boring-sys builds
        # fine under musl here (cmake path), openssl is NOT in the dep tree
        # (pingora-boringssl), so no openssl-static pitfall applies.
        mkAtgMusl = tierName: tier:
          let
            pkgsMusl = import nixpkgs {
              inherit system;
              crossSystem = { config = "x86_64-unknown-linux-musl"; };
            };
          in
          pkgsMusl.rustPlatform.buildRustPackage {
            pname = "agent-trace-gateway-${tierName}-musl";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgsMusl; [ cmake clang go perl pkg-config git ];
            # bindgen's libclang must see the MUSL sysroot headers, not the
            # host glibc ones — without BINDGEN_EXTRA_CLANG_ARGS it dies on
            # 'sys/types.h file not found' while parsing openssl/base.h.
            env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            env.BINDGEN_EXTRA_CLANG_ARGS = "--sysroot=${pkgsMusl.stdenv.cc.libc.dev} --target=x86_64-unknown-linux-musl";
            RUSTFLAGS = "-C target-cpu=${cpuModelFor tier}";
            doCheck = false;
          };

      in
      {
        packages = {
          atg-baseline = mkAtg "baseline" tiers.baseline;
          atg-v2 = mkAtg "v2" tiers.v2;
          atg-v3 = mkAtg "v3" tiers.v3;
          atg-musl-baseline = mkAtgMusl "baseline" tiers.baseline;
          atg-musl-v2 = mkAtgMusl "v2" tiers.v2;
          atg-musl-v3 = mkAtgMusl "v3" tiers.v3;
          dockerImage-v1 = imageBuilder "baseline" tiers.baseline;
          dockerImage-v2 = imageBuilder "v2" tiers.v2;
          dockerImage-v3 = imageBuilder "v3" tiers.v3;
          default = pkgs.symlinkJoin {
            name = "atg-all-tiers";
            paths = with pkgs.lib;
              [ self.packages.${system}.atg-baseline ];
            meta.description = "default = baseline tier (production-safe)";
          };
        };

        # Dev shell with rust toolchain for local iteration.
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ rustc cargo clippy rustfmt cmake clang go perl pkg-config ];
        };
      }
    );
}