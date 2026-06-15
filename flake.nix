# flake.nix — reproducible OCI image builds for sv2-apps release binaries.
#
# WHAT THIS PRODUCES
#   packages.<system>.{pool_sv2, jd_client_sv2, translator_sv2}            -- release binaries
#   packages.<system>.container.{pool_sv2, jd_client_sv2, translator_sv2}  -- OCI images (gzipped tarballs)
#   devShells.<system>.default                                             -- dev env mirroring docker/Dockerfile
#
# RELATIONSHIP TO EXISTING DOCKER PIPELINE
#   This flake lives ALONGSIDE docker/Dockerfile and .github/workflows/docker-release.yaml,
#   not as a replacement. It is opt-in / experimental. The existing Buildx + QEMU pipeline
#   continues to be the source of truth until reproducibility of this path is verified.
#
# DESIGN
#   - Crane (https://crane.dev) for Rust builds; Fenix for toolchain pinning from rust-toolchain.toml.
#   - dockerTools.buildLayeredImage produces OCI tarballs (no Docker daemon needed at build time).
#   - Multi-arch is achieved at the CI level via native runners per arch (see ci-nix.yml),
#     not via cross-compilation here. Each Nix system builds a single-arch image; CI stitches
#     them with `docker manifest create`.
#   - NO `runAsRoot` is used in image config — sidesteps nixpkgs#416467 on aarch64-linux.
#   - The repo has TWO cargo workspaces (pool-apps/, miner-apps/) plus the stratum-apps
#     "shim" crate. Each binary is built from its respective workspace lockfile.
#
# CONTEXT
#   See docker/REPRODUCIBILITY.md and the wiki article at
#   ~/wiki/topics/nixos-reproducible-builds-bitcoin/wiki/topics/sv2-apps-oci-reproducibility-feasibility.md
#
# FIRST-BUILD TODOs
# NOTE
#   The toolchain hash below pins channel = "1.85.0" + rustfmt/clippy/rust-analyzer
#   (matches rust-toolchain.toml). If rust-toolchain.toml changes, replace the sha256
#   with all-A's, run `nix build`, and copy the "got" value from the resulting error.
#
#   The stratum-core git dependency (branch=main, currently rev 083b217...) is
#   resolved automatically by Crane from each workspace's Cargo.lock — no manual
#   outputHashes entries are required. If a future Cargo.lock pins a NEW git source
#   that Crane cannot auto-resolve (rare), Crane will print the expected
#   `outputHashes` entry; paste it into the corresponding `vendorCargoDeps` block.

{
  description = "Stratum V2 sv2-apps reproducible OCI image builds";

  inputs = {
    # Pinned to nixos-25.11 (Crane requires nixpkgs >= 25.11 as of v0.21).
    # Bump deliberately when you want newer toolchains or dockerTools fixes.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

    flake-utils.url = "github:numtide/flake-utils";

    crane = {
      url = "github:ipetkov/crane";
      # Crane no longer requires `inputs.nixpkgs.follows` since v0.18; it reads pkgs from the caller.
    };

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, crane, fenix }:
    flake-utils.lib.eachSystem [
      "x86_64-linux"
      "aarch64-linux"
      # darwin systems are dev-shell only — OCI images target linux.
      "x86_64-darwin"
      "aarch64-darwin"
    ] (system:
      let
        pkgs = import nixpkgs { inherit system; };
        isLinux = pkgs.stdenv.hostPlatform.isLinux;

        # ---- Rust toolchain pinned via rust-toolchain.toml ----
        rustToolchain = fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          # Hash for rust-toolchain.toml (channel = 1.85.0 + rustfmt/clippy/rust-analyzer).
          # Regenerate when rust-toolchain.toml changes by replacing with all-A's and
          # copying the "got" value from the resulting `nix build` error.
          sha256 = "sha256-AJ6LX/Q/Er9kS15bn9iflkUwcgYqRQxiOIL2ToVAXaU=";
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # ---- Source filtering ----
        # Crane's default cleanCargoSource drops everything that isn't .rs/Cargo.{toml,lock}.
        # We need the full repo because pool-apps depends on bitcoin-core-sv2 and stratum-apps
        # via path. Use a permissive filter that keeps Rust + workspace metadata.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            (craneLib.filterCargoSources path type)
            # Keep config-examples + READMEs referenced by Cargo.toml `readme` keys.
            || (builtins.match ".*/README\\.md$" path != null)
            || (builtins.match ".*/config-examples/.*" path != null);
          name = "sv2-apps-source";
        };

        # ---- Common build inputs ----
        nativeBuildInputs = with pkgs; [
          pkg-config
          capnproto      # build-time, matches Dockerfile builder stage
          clang
        ];

        buildInputs = with pkgs; [
          # Add openssl/zlib here if a transitive dep complains on first build.
        ];

        # ---- Per-workspace common args ----
        # Each binary lives in its own workspace; we build deps once per workspace.

        commonArgsPool = {
          inherit src nativeBuildInputs buildInputs;
          pname = "sv2-pool-apps-deps";
          version = "0.4.0";
          cargoLock = ./pool-apps/Cargo.lock;
          cargoToml = ./pool-apps/Cargo.toml;
          # Build from the workspace root.
          cargoExtraArgs = "--manifest-path pool-apps/Cargo.toml";
          # Crane auto-resolves git deps from the lockfile (Cargo.lock pins
          # stratum-core to a specific rev). No outputHashes entries needed
          # for the stratum-core branch=main case.
          cargoVendorDir = craneLib.vendorCargoDeps {
            src = ./.;
            cargoLock = ./pool-apps/Cargo.lock;
          };
        };

        commonArgsMiner = {
          inherit src nativeBuildInputs buildInputs;
          pname = "sv2-miner-apps-deps";
          version = "0.3.0";
          cargoLock = ./miner-apps/Cargo.lock;
          cargoToml = ./miner-apps/Cargo.toml;
          cargoExtraArgs = "--manifest-path miner-apps/Cargo.toml";
          cargoVendorDir = craneLib.vendorCargoDeps {
            src = ./.;
            cargoLock = ./miner-apps/Cargo.lock;
          };
        };

        # ---- Dependency-only builds (cached separately per workspace) ----
        cargoArtifactsPool = craneLib.buildDepsOnly commonArgsPool;
        cargoArtifactsMiner = craneLib.buildDepsOnly commonArgsMiner;

        # ---- Per-binary release builds ----
        pool_sv2 = craneLib.buildPackage (commonArgsPool // {
          pname = "pool_sv2";
          version = "0.4.0";
          cargoArtifacts = cargoArtifactsPool;
          cargoExtraArgs = "--manifest-path pool-apps/Cargo.toml -p pool_sv2 --bin pool_sv2";
          # Skip workspace-wide tests at image-build time; CI runs them separately.
          doCheck = false;
        });

        jd_client_sv2 = craneLib.buildPackage (commonArgsMiner // {
          pname = "jd_client_sv2";
          version = "0.3.0";
          cargoArtifacts = cargoArtifactsMiner;
          cargoExtraArgs = "--manifest-path miner-apps/Cargo.toml -p jd_client_sv2 --bin jd_client_sv2";
          doCheck = false;
        });

        translator_sv2 = craneLib.buildPackage (commonArgsMiner // {
          pname = "translator_sv2";
          version = "0.3.0";
          cargoArtifacts = cargoArtifactsMiner;
          cargoExtraArgs = "--manifest-path miner-apps/Cargo.toml -p translator_sv2 --bin translator_sv2";
          doCheck = false;
        });

        # ---- OCI image builder ----
        # Single-arch per system. CI stitches with `docker manifest create`.
        # NO runAsRoot — sidesteps nixpkgs#416467 on aarch64-linux GitHub runners.
        mkContainer = { name, pkg, binName }:
          pkgs.dockerTools.buildLayeredImage {
            inherit name;
            tag = "latest";  # CI retags with ${github.ref_name}-${arch}.
            # Epoch+1 timestamp (the +1 sidesteps a historical dockerTools quirk that
            # would otherwise treat 0 as "unset" and fall back to build time).
            created = "1970-01-01T00:00:01Z";

            architecture =
              if pkgs.stdenv.hostPlatform.system == "x86_64-linux" then "amd64"
              else if pkgs.stdenv.hostPlatform.system == "aarch64-linux" then "arm64"
              else throw "OCI images only supported on linux systems; got ${pkgs.stdenv.hostPlatform.system}";

            contents = [
              pkg
              # gettext provides envsubst — kept for parity with the existing Dockerfile's
              # runtime stage, which installs gettext-base for config templating.
              pkgs.gettext
              # bash + coreutils so an entrypoint sh -c wrapper works (the Dockerfile uses
              # ENTRYPOINT ["/bin/sh", "-c", ...]); also handy for `docker exec` debugging.
              pkgs.bash
              pkgs.coreutils
              # CA bundle for any TLS calls the binaries make.
              pkgs.cacert
            ];

            config = {
              # Direct binary invocation. If you need the existing Dockerfile's
              # `sh -c "/app/${APP}"` envsubst-on-args behavior, swap to:
              #   Entrypoint = [ "${pkgs.bash}/bin/sh" "-c" "${pkg}/bin/${binName} \"$@\"" "--" ];
              Cmd = [ "${pkg}/bin/${binName}" ];
              Env = [
                "PATH=/bin:/usr/bin"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              WorkingDir = "/";
              Labels = {
                "org.opencontainers.image.source" = "https://github.com/stratum-mining/sv2-apps";
                "org.opencontainers.image.licenses" = "MIT OR Apache-2.0";
              };
            };
          };

      in {
        packages = {
          inherit pool_sv2 jd_client_sv2 translator_sv2;

          # Containers only build on linux; on darwin these attrs are absent.
          container = pkgs.lib.optionalAttrs isLinux {
            pool_sv2 = mkContainer {
              name = "stratumv2/pool_sv2";
              pkg = pool_sv2;
              binName = "pool_sv2";
            };
            jd_client_sv2 = mkContainer {
              name = "stratumv2/jd_client_sv2";
              pkg = jd_client_sv2;
              binName = "jd_client_sv2";
            };
            translator_sv2 = mkContainer {
              name = "stratumv2/translator_sv2";
              pkg = translator_sv2;
              binName = "translator_sv2";
            };
          };

          default = pool_sv2;
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = pkgs.lib.optionals isLinux [
            # Pull in build inputs from one of the binaries on linux for full parity;
            # on darwin we just want toolchain + tools.
            pool_sv2
          ];
          packages = with pkgs; [
            rustToolchain
            capnproto
            pkg-config
            clang
            gettext        # envsubst — runtime parity with Dockerfile
            cargo-watch
            cargo-nextest
          ];
          # Helpful for rust-analyzer integration.
          RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        };

        # `nix flake check` runs this; keep cheap so CI's `nix flake check --no-build` works.
        checks = pkgs.lib.optionalAttrs isLinux {
          inherit pool_sv2 jd_client_sv2 translator_sv2;
        };
      });
}
