{
  description = "cclens — a lens onto your Claude Code usage";

  # Advertise the shared Cachix cache so `nix run github:lambdalisue/cclens`
  # pulls prebuilt artifacts instead of compiling from source. Trusted users
  # get it automatically; others are prompted to accept it on first use.
  nixConfig = {
    extra-substituters = [ "https://cclens.cachix.org" ];
    extra-trusted-public-keys = [
      "cclens.cachix.org-1:0QUNU6PuVyf+yXOvg3n1rd3FksBoB3s3/Jty50iKRNQ="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    nix-github-release = {
      url = "github:lambdalisue/nix-github-release/v0.1.0";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, nix-github-release }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Single source of the Rust version, shared by the dev shell and the
        # package build (rust-toolchain.toml).
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          # rusqlite is built with the `bundled` feature, so SQLite compiles from
          # C against the stdenv C toolchain — no system sqlite/pkg-config needed.
        };
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # The tag this flake was fetched at, from VERSION; see
        # github:lambdalisue/nix-github-release. Only the package is stamped
        # with it, so the dependency layer above stays cached across releases.
        version = nix-github-release.lib.version { inherit self; };
        cclens = craneLib.buildPackage (nix-github-release.lib.stampCargo { inherit pkgs version; }
          (commonArgs // {
            inherit cargoArtifacts;
            # The change-date tests build throwaway git repositories.
            nativeCheckInputs = [ pkgs.git ];
          }));
      in
      {
        # `nix build` / `nix run github:lambdalisue/cclens -- summary`; pin a
        # release with `github:lambdalisue/cclens/vX.Y.Z`.
        packages.default = cclens;
        apps.default = flake-utils.lib.mkApp { drv = cclens; };

        # `nix develop` — the pinned tools, same ones CI uses via `just`.
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            just
            git
            jq
            sqlite # the `sqlite3` CLI, handy for poking at the store
            nixpkgs-fmt
          ];
          env.RUST_BACKTRACE = "1";
          shellHook = ''
            echo "cclens dev shell — $(rustc --version)"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
