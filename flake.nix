{
  description = "cresset-tools/infra: NixOS configurations for every host I run";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    determinate.url = "https://flakehub.com/f/DeterminateSystems/determinate/*";
    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixos-anywhere = {
      url = "github:nix-community/nixos-anywhere";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # rustc 1.96 for sconce (nixpkgs default lags; pinned from its toolchain file).
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Encrypted secrets for hosts/demo (the flake's first secrets framework).
    sops-nix = {
      url = "github:Mic92/sops-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The (private) relay for `bougie share`, built on hosts/bougie-relay via
    # rustPlatform.buildRustPackage. `flake = false`: it's a plain Cargo
    # project, and keeping it a source input (not vendored) leaves the relay
    # closed. Distributed privately via FlakeHub (see the repo's flakehub-push
    # workflow), so this + the box fetch it over a FlakeHub token instead of an
    # SSH deploy key: CI authenticates via GitHub OIDC (id-token), and locally
    # `determinate-nixd login` provides the token for `nix run .#switch`.
    bougie-relay = {
      url = "https://flakehub.com/f/cresset-tools/bougie-relay/*.tar.gz";
      flake = false;
    };
    # Push-based CD: .github/workflows/deploy.yml builds each host on the runner
    # and activates it over SSH with deploy-rs (magic rollback). Replaces the
    # per-box pull `system.autoUpgrade`.
    deploy-rs = {
      url = "github:serokell/deploy-rs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs@{ self, nixpkgs, determinate, disko, nixos-anywhere, rust-overlay, sops-nix, bougie-relay, deploy-rs }:
    let
      # CAX11 is aarch64. The deploy/switch helper apps run on the
      # operator's laptop too, so we expose them on both common arches.
      system = "x86_64-linux";
      # Overlay nixpkgs so `pkgs.nix` (and anything built against it, e.g.
      # nixos-rebuild) is Determinate Nix rather than upstream Nix.
      pkgs = import nixpkgs {
        inherit system;
        overlays = [
          (final: prev: {
            nix = determinate.inputs.nix.packages.${system}.default;
          })
          # rust-bin.* for pinning sconce's rustc 1.96 (see demo-images.nix).
          rust-overlay.overlays.default
        ];
      };
      # Every directory under ./hosts/ becomes a nixosConfigurations entry.
      # Each host dir must contain configuration.nix; disko.nix is optional
      # (omit on hosts where the disk layout was set up another way).
      hostNames = builtins.attrNames
        (nixpkgs.lib.filterAttrs (_: v: v == "directory")
          (builtins.readDir ./hosts));

      mkHost = name:
        let
          hostDir = ./hosts/${name};
          hasDisko = builtins.pathExists (hostDir + "/disko.nix");
          # Per-host architecture: hosts/<name>/system holds the system
          # string (e.g. "x86_64-linux" for the CX-line telemetry box);
          # absent means aarch64-linux, the CAX default this flake grew
          # up with.
          systemFile = hostDir + "/system";
          hostSystem =
            if builtins.pathExists systemFile
            then nixpkgs.lib.removeSuffix "\n" (builtins.readFile systemFile)
            else "aarch64-linux";
        in nixpkgs.lib.nixosSystem {
          system = hostSystem;
          # Pass the flake inputs (and `self`, for `self.packages`) to host
          # modules — hosts/demo needs inputs.sops-nix + the image packages.
          specialArgs = { inherit inputs; };
          modules =
            [ (hostDir + "/configuration.nix") ]
            ++ nixpkgs.lib.optionals hasDisko [
              disko.nixosModules.disko
              (hostDir + "/disko.nix")
            ];
        };
    in {
      nixosConfigurations = nixpkgs.lib.genAttrs hostNames mkHost;

      # ---- Push-based CD (deploy-rs) ----
      # `.github/workflows/deploy.yml` runs `deploy .#<host>` on merge to main:
      # build the closure on the runner (FlakeHub OIDC covers the private
      # bougie-relay input), copy it, and activate over SSH with magic rollback
      # (reverts if the box goes unreachable). This replaces per-box pull
      # `system.autoUpgrade`, so no host needs a FlakeHub token — the runner
      # holds the auth and hands each box a finished closure.
      #
      # The persistent boxes are wired (origin is aarch64 → deploy.yml builds it
      # on an arm64 runner). demo (heavy Nix-built OCI images) and mageos-testing
      # (throwaway) still deploy by hand via `nix run .#switch`.
      deploy = {
        sshUser = "root";
        magicRollback = true;
        autoRollback = true;
        nodes =
          let
            node = system: hostname: name: {
              inherit hostname;
              profiles.system.path =
                deploy-rs.lib.${system}.activate.nixos self.nixosConfigurations.${name};
            };
          in {
            bougie-relay = node "x86_64-linux" "2.28.9.32" "bougie-relay"; # *.bougie.show apex has no A record
            bougierepo = node "x86_64-linux" "bougierepo.com" "bougierepo";
            telemetry = node "x86_64-linux" "telemetry.bougie.tools" "telemetry";
            origin = node "aarch64-linux" "origin.bougie.tools" "origin"; # aarch64 CAX11 dist-index/mirror
          };
      };

      # Nix-built OCI images for the demo host (built here, loaded via
      # oci-containers imageFile). Also `nix build .#sconceImage` to inspect.
      packages.${system} =
        let images = import ./demo-images.nix { inherit pkgs; };
        in {
          inherit (images) sconce sconceImage phpRuntime magentoImage;
        };

      apps.${system} = {
        # `nix run .#deploy -- <host> <ip>` from a fresh laptop. Wraps
        # nixos-anywhere with the named host's config.
        #
        # `--flake` points at THIS flake by store path (`${self}`), not `.#`, so
        # the app works from any working directory — `nix run ~/infra#deploy`
        # from elsewhere used to fail with "not part of a flake" because the `.`
        # resolved to the caller's cwd. It also guarantees the app and the host
        # config come from the same evaluation rather than whatever flake happens
        # to sit in `$PWD`.
        deploy = {
          type = "app";
          program = toString (pkgs.writeShellScript "deploy" ''
            set -euo pipefail
            if [ "$#" -lt 2 ]; then
              echo "usage: nix run ~/infra#deploy -- <host> <ip-or-hostname> [extra-flags...]" >&2
              exit 2
            fi
            host="$1"; target="$2"; shift 2
            exec ${nixos-anywhere.packages.${system}.default}/bin/nixos-anywhere \
              --flake "${self}#$host" \
              --target-host "root@$target" \
              --print-build-logs \
              "$@"
          '');
        };

        # `nix run .#switch -- <host> <ip>` for incremental updates.
        # Uses nixpkgs's nixos-rebuild rather than the system PATH (which
        # may not have it, e.g. the operator running from Debian). Builds
        # on the target itself so cross-arch concerns (laptop is x86_64,
        # box is aarch64) don't matter. `--flake` is the absolute `${self}`
        # store path for the same reason as `deploy` above — cwd-independent.
        switch = {
          type = "app";
          program = toString (pkgs.writeShellScript "switch" ''
            set -euo pipefail
            if [ "$#" -lt 2 ]; then
              echo "usage: nix run ~/infra#switch -- <host> <ip-or-hostname> [extra-flags...]" >&2
              exit 2
            fi
            host="$1"; target="$2"; shift 2
            exec ${pkgs.nixos-rebuild}/bin/nixos-rebuild switch \
              --flake "${self}#$host" \
              --target-host "root@$target" \
              --build-host "root@$target" \
              --use-substitutes \
              "$@"
          '');
        };

        # The pinned deploy-rs CLI for the CD workflow (deploy.yml). Distinct
        # from `.#deploy` (nixos-anywhere, first-time provisioning) and `.#switch`
        # (build-on-target); this one builds on the runner and pushes closures.
        deploy-rs = {
          type = "app";
          program = "${deploy-rs.packages.${system}.default}/bin/deploy";
        };
      };
    };
}
