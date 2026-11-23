{ pkgs }:
pkgs.rustPlatform.buildRustPackage {
  pname = "preview";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = pkgs.lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      # The unit tests read tests/config.toml; without it they fail to build.
      ./tests
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = true;
  meta = {
    description = "Preview environments: host CLI and webhook daemon, and the guest agent";
    mainProgram = "preview";
  };
}
