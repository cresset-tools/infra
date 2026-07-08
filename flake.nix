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
  };

  outputs = inputs@{ self, nixpkgs, determinate, disko, nixos-anywhere, rust-overlay, sops-nix }:
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
        deploy = {
          type = "app";
          program = toString (pkgs.writeShellScript "deploy" ''
            set -euo pipefail
            if [ "$#" -lt 2 ]; then
              echo "usage: nix run .#deploy -- <host> <ip-or-hostname> [extra-flags...]" >&2
              exit 2
            fi
            host="$1"; target="$2"; shift 2
            exec ${nixos-anywhere.packages.${system}.default}/bin/nixos-anywhere \
              --flake ".#$host" \
              --target-host "root@$target" \
              --print-build-logs \
              "$@"
          '');
        };

        # `nix run .#switch -- <host> <ip>` for incremental updates.
        # Uses nixpkgs's nixos-rebuild rather than the system PATH (which
        # may not have it, e.g. the operator running from Debian). Builds
        # on the target itself so cross-arch concerns (laptop is x86_64,
        # box is aarch64) don't matter.
        switch = {
          type = "app";
          program = toString (pkgs.writeShellScript "switch" ''
            set -euo pipefail
            if [ "$#" -lt 2 ]; then
              echo "usage: nix run .#switch -- <host> <ip-or-hostname> [extra-flags...]" >&2
              exit 2
            fi
            host="$1"; target="$2"; shift 2
            exec ${pkgs.nixos-rebuild}/bin/nixos-rebuild switch \
              --flake ".#$host" \
              --target-host "root@$target" \
              --build-host "root@$target" \
              --use-substitutes \
              "$@"
          '');
        };
      };
    };
}
