{ pkgs }:
let
  web = pkgs.buildNpmPackage {
    pname = "cresset-view-web";
    version = "0.1.0";
    src = pkgs.lib.fileset.toSource {
      root = ./web;
      fileset = pkgs.lib.fileset.unions [
        ./web/index.html
        ./web/package.json
        ./web/package-lock.json
        ./web/tsconfig.json
        ./web/src
      ];
    };
    npmDepsHash = "sha256-ST8Bo1mKZpa8eP4j9MEhiHIiLd4snyrCk0F8mtm76t0=";
    # Run `npm test` in the sandbox. The revision graph layout carries lane state across
    # pages, and a mistake there does not throw -- it draws a history that is subtly not the
    # one in the repository. A check that only runs on someone's laptop does not protect that.
    doCheck = true;
    checkPhase = ''
      runHook preCheck
      npm run test
      runHook postCheck
    '';
    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  };
in
pkgs.rustPlatform.buildRustPackage {
  pname = "cresset-view";
  version = "0.1.0";
  src = pkgs.lib.fileset.toSource {
    root = ./.;
    fileset = pkgs.lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };
  cargoLock = {
    lockFile = ./Cargo.lock;
    outputHashes = {
      "jj-lib-0.43.0" = "sha256-XgBq2ZN34iWlwKVgW7Syr46KUdt7pJuSDd/J6QWJwwQ=";
    };
  };
  postInstall = ''
    mkdir -p $out/share/cresset-view
    cp -r ${web}/* $out/share/cresset-view/
  '';
  doCheck = true;
}
