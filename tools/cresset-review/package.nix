{ pkgs }:
pkgs.rustPlatform.buildRustPackage {
  pname = "cresset-review";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = pkgs.lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      # Without this the integration tests are absent from the sandbox and `cargo test` reports
      # success having run only the unit tests — the same trap cresset-view documents. The
      # identity-pinning test lives in there, and it is the one that must not silently vanish.
      ./tests
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;
  doCheck = true;
  meta = {
    description = "Read and answer Cresset review comments from a terminal";
    mainProgram = "cresset-review";
  };
}
