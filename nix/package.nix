{
  lib,
  rustPlatform,
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

  cargoHash = "sha256-r4/DPK04imr8d9mYmBLdf5xXeaOW7k8Yo8zNEZL7E7E=";

  meta = {
    description = "PTY-based CLI testing framework";
    homepage = "https://github.com/kexi/pitty";
    license = lib.licenses.mit;
    mainProgram = "pitty";
    platforms = lib.platforms.unix;
  };
}
