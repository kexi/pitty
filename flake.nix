{
  description = "pitty - PTY-based CLI testing framework";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rustfmt" "clippy" ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.pkg-config
            pkgs.just
            pkgs.lefthook
            pkgs.gitleaks
          ];
          # Install the lefthook git hooks on entering the dev shell so the
          # gitleaks pre-commit tripwire (lefthook.yml) is wired up without a
          # manual step. `lefthook install` is idempotent; silence it when there
          # is no .git dir (e.g. a tarball checkout) so the shell still loads.
          shellHook = ''
            if [ -d .git ]; then lefthook install >/dev/null 2>&1 || true; fi
          '';
        };
      });
}
