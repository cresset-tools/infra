{
  description = "cresset-tools/infra: NixOS configurations for every host I run";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    nixos-anywhere = {
      url = "github:nix-community/nixos-anywhere";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, disko, nixos-anywhere }:
    let
      # CAX11 is aarch64. The deploy/switch helper apps run on the
      # operator's laptop too, so we expose them on both common arches.
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      hostSystem = "aarch64-linux";

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
        in nixpkgs.lib.nixosSystem {
          system = hostSystem;
          modules =
            [ (hostDir + "/configuration.nix") ]
            ++ nixpkgs.lib.optionals hasDisko [
              disko.nixosModules.disko
              (hostDir + "/disko.nix")
            ];
        };
    in {
      nixosConfigurations = nixpkgs.lib.genAttrs hostNames mkHost;

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
              "$@"
          '');
        };

        # `nix run .#switch -- <host> <ip>` for incremental updates.
        switch = {
          type = "app";
          program = toString (pkgs.writeShellScript "switch" ''
            set -euo pipefail
            if [ "$#" -lt 2 ]; then
              echo "usage: nix run .#switch -- <host> <ip-or-hostname> [extra-flags...]" >&2
              exit 2
            fi
            host="$1"; target="$2"; shift 2
            exec nixos-rebuild switch \
              --flake ".#$host" \
              --target-host "root@$target" \
              --use-substitutes \
              "$@"
          '');
        };
      };
    };
}
