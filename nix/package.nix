{
  lib,
  rustPlatform,
  jq,
  source ? lib.cleanSourceWith {
    src = lib.cleanSource ../.;
    filter =
      path: type:
      let
        rel = lib.removePrefix "${toString ../.}/" (toString path);
      in
      !(lib.hasPrefix "target/" rel
        || lib.hasPrefix "logs/" rel
        || lib.hasPrefix ".direnv/" rel);
  },
  version ? (builtins.fromTOML (builtins.readFile ../Cargo.toml)).package.version,
}:

rustPlatform.buildRustPackage rec {
  pname = "pitty";
  src = source;
  inherit version;

  # Rotates on *any* Cargo.lock change, not just dependency upgrades: nixpkgs'
  # vendor staging hashes Cargo.lock itself, so a release commit that bumps only
  # the `pitty` version line invalidates this too. That is exactly how v1.2.0 and
  # v1.2.1 shipped an unbuildable flake. Re-run `nix build .#default` after every
  # lock change and paste the hash Nix reports; CI's `nix-build` job gates it.
  cargoHash = "sha256-UOdvnKhzoj1VGXEf1jHPER3GuQDtb3kqowtNGS3yxGI=";

  # checkPhase runs the release-gate contract tests, which execute
  # .github/scripts/wait-for-ci.sh against a fake `gh`; the script needs jq.
  nativeCheckInputs = [ jq ];

  meta = {
    description = "PTY-based CLI testing framework";
    homepage = "https://github.com/kexi/pitty";
    license = lib.licenses.mit;
    mainProgram = "pitty";
    platforms = lib.platforms.unix;
  };
}
