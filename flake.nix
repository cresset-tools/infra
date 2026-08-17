{
  description = "cresset-tools/infra: NixOS configurations for every host I run";

  inputs = {
    # Back on `nixos-unstable`. It was pinned to 753cc8a for one reason: a weekly bump
    # had merged a revision where authentik does not build, and nothing was checking.
    # `check.yml` now builds every deployed host on the PR, and the flake-update PR is
    # authored by an App so those checks actually run — so a bump that breaks a host
    # fails its own PR instead of reaching main, the monorepo, and the box.
    #
    # The lock still holds whatever last passed that gate; removing the pin only means a
    # bump is now allowed to be PROPOSED, not that it is trusted.
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
    # The cresset-sync worker, distributed the SAME way as bougie-relay: the
    # PRIVATE cresset-tools/cresset-sync repo publishes itself to FlakeHub from
    # its own flakehub-push workflow (GitHub OIDC), and hosts/internal builds it
    # via buildRustPackage. `flake = false`: it's a plain Cargo project kept a
    # source input, so it stays buildable from the standalone cresset-tools/infra
    # clone without an SSH deploy key.
    #
    # That repo is a publication mirror of `operations/sync` in the canonical
    # monorepo, exported by the worker itself — the source of truth is the
    # monorepo, not GitHub. Publishing has to originate from a GitHub repository
    # because flakehub-push authenticates over GitHub Actions OIDC; private repo
    # + private artifact keeps the worker as internal as that allows.
    #
    # NOTE: this input resolves only once that repo exists and has published at
    # least once; until then `nix flake lock`/`metadata` cannot fetch it — which
    # blocks evaluation of EVERY host in this flake, not just internal. See the
    # bootstrap runbook in the monorepo's docs/SYNC_WORKER.md.
    cresset-sync = {
      url = "https://flakehub.com/f/cresset-tools/cresset-sync/*.tar.gz";
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

  outputs = inputs@{ self, nixpkgs, determinate, disko, nixos-anywhere, sops-nix, bougie-relay, cresset-sync, deploy-rs }:
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
        ];
      };
      # The flakeref the deploy/switch apps hand to nixos-anywhere/nixos-rebuild.
      #
      # These apps embed a store path rather than `.#` so they work from any working
      # directory (see the `deploy` app below). Getting that right in the monorepo took
      # two goes, because a store path is NOT usable as a flakeref when the flake sits
      # in a subdirectory:
      #
      #   nix ... '/nix/store/<hash>-source/operations/infra#internal'
      #   error: installable '/nix/store/<hash>-source' does not correspond to a
      #          Nix language value
      #
      # Nix normalises any path under /nix/store back to its store ROOT and silently
      # drops the subpath, so `${self}` — which does point at the subdirectory — loses
      # exactly the part that matters. The subdirectory has to be carried as `?dir=`
      # on an explicit `path:` ref instead. This is what killed the first `internal`
      # deploy, after kexec but before disko, so nothing was lost but the wall time.
      #
      # `sourceInfo.outPath` is the reliable discriminator: it is always the store root
      # of the copied source, whatever `self.outPath` happens to be. A standalone
      # cresset-tools/infra clone has flake.nix there; the monorepo does not.
      srcRoot = self.sourceInfo.outPath;
      selfFlake =
        if builtins.pathExists (srcRoot + "/flake.nix")
        then "path:${srcRoot}"
        else "path:${srcRoot}?dir=operations/infra";

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
            internal = node "x86_64-linux" "internal.cresset.tools" "internal";
          };
      };

      # Tests for the canonical repository's git hooks.
      #
      # Run by `nix flake check`, which check.yml already runs on every PR. The hook is read
      # from the SAME file hosts/internal/git-canonical.nix installs, so this cannot pass
      # against a copy that has drifted from what the host runs.
      checks.${system}.canonical-review-hooks =
        let
          hook = pkgs.writeShellApplication {
            name = "post-receive";
            runtimeInputs = [ pkgs.git ];
            text = builtins.readFile ./hosts/internal/hooks/post-receive;
          };
        in
        pkgs.runCommand "canonical-review-hooks" {
          nativeBuildInputs = [ pkgs.git pkgs.jujutsu ];
        } ''
          set -eu
          export HOME=$TMPDIR JJ_CONFIG=/dev/null
          cd "$TMPDIR"

          git init -q --bare -b main canonical.git
          install -m 0755 ${hook}/bin/post-receive canonical.git/hooks/post-receive
          bare="$TMPDIR/canonical.git"
          g() { git --git-dir="$bare" "$@"; }

          jj git init --colocate work >/dev/null 2>&1
          cd work
          jj config set --repo user.name Test >/dev/null
          jj config set --repo user.email test@example.com >/dev/null
          echo base > f.txt
          jj commit -m base >/dev/null
          jj bookmark set main -r @- >/dev/null
          jj git remote add canonical "$bare" >/dev/null
          jj git push -b main >/dev/null 2>&1

          # 1. A change pushed for review is pinned as patch set 1.
          jj new main -m "a change" >/dev/null
          echo one > g.txt
          jj bookmark set review/thing -r @ >/dev/null
          jj git push -b review/thing >/dev/null 2>&1
          cid=$(g for-each-ref --format='%(refname)' 'refs/changes/**' | head -1 | cut -d/ -f3)
          test -n "$cid" || { echo "FAIL: no patch set was recorded"; exit 1; }
          test "$(g show "refs/changes/$cid/1:g.txt")" = one || { echo "FAIL: patch set 1 content"; exit 1; }

          # 2. Amending records a SECOND patch set under the same change.
          echo two >> g.txt
          jj bookmark set review/thing -r @ >/dev/null
          jj git push -b review/thing >/dev/null 2>&1
          test "$(g show "refs/changes/$cid/2:g.txt" | tr '\n' ' ')" = "one two " \
            || { echo "FAIL: patch set 2 content"; exit 1; }

          # 3. The superseded patch set is on no branch, yet survives gc. This is the whole
          #    reason the hook exists: jj discards the old commit on amend.
          test "$(g branch --contains "$(g rev-parse "refs/changes/$cid/1")" | wc -l)" = 0 \
            || { echo "FAIL: patch set 1 should be unreachable from any branch"; exit 1; }
          g gc --prune=now --quiet
          test "$(g cat-file -t "refs/changes/$cid/1")" = commit \
            || { echo "FAIL: patch set 1 did not survive gc"; exit 1; }

          # 4. Re-pushing an unchanged bookmark must not mint a duplicate patch set.
          jj git push -b review/thing >/dev/null 2>&1 || true
          test "$(g for-each-ref 'refs/changes/**' | wc -l)" = 2 \
            || { echo "FAIL: re-push created a duplicate patch set"; exit 1; }

          # 5. A commit with no change-id (plain git) is noted, not fatal.
          cd "$TMPDIR"
          git clone -q "$bare" plain
          cd plain
          git checkout -q -b review/plain main
          echo x > plain.txt
          git add -A
          git -c user.name=P -c user.email=p@e.com commit -qm "plain"
          git push -q origin review/plain 2>err.txt || { echo "FAIL: push rejected"; exit 1; }
          grep -q "no change-id" err.txt || { echo "FAIL: expected a note about the missing change-id"; exit 1; }

          # 6. Patch sets must stay OUT of jj's namespace.
          #
          # This is the property the whole review design rests on, and it is easy to break by
          # "tidying" patch sets into refs/heads so jj can see them. If they arrive as
          # bookmarks, every version of a change becomes a visible commit sharing one change
          # id and jj refuses to resolve it -- `Change ID is divergent` -- in the very
          # repository that needs to resolve change ids. Measured, not assumed: doing it that
          # way produced 2 divergent changes from 2 changes under review.
          cd "$TMPDIR"
          jj git clone --colocate "$bare" viewer >/dev/null 2>&1
          cd viewer
          git fetch --quiet origin "+refs/changes/*:refs/changes/*"
          test "$(git for-each-ref 'refs/changes/**' | wc -l)" = 2 \
            || { echo "FAIL: patch sets did not reach the viewer"; exit 1; }
          test "$(jj log -r 'divergent()' --no-graph -T '"x"' 2>/dev/null | wc -c)" = 0 \
            || { echo "FAIL: fetching patch sets made a change divergent"; exit 1; }
          jj log -r "$cid" --no-graph -T '"ok"' >/dev/null 2>&1 \
            || { echo "FAIL: the change id is no longer addressable"; exit 1; }
          # And both versions are still readable, which is the point of keeping them.
          test "$(git show "refs/changes/$cid/1:g.txt")" = one \
            || { echo "FAIL: patch set 1 unreadable from the viewer"; exit 1; }

          echo "all canonical hook checks passed"
          touch $out
        '';

      # Nix-built OCI images for the demo host (built here, loaded via
      # oci-containers imageFile). sconce is not among them — it comes from
      # ghcr.io/cresset-tools/sconce, pulled by tag (see demo-images.nix).
      packages.${system} =
        let images = import ./demo-images.nix { inherit pkgs; };
        in {
          inherit (images) phpRuntime magentoImage;
          cresset-view = import ./tools/cresset-view/package.nix { inherit pkgs; };
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
              --flake "${selfFlake}#$host" \
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
              --flake "${selfFlake}#$host" \
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

      # deploy-rs must also run on the arm64 CI runner (origin builds there);
      # the operator apps (deploy/switch) stay x86-only.
      apps.aarch64-linux.deploy-rs = {
        type = "app";
        program = "${deploy-rs.packages.aarch64-linux.default}/bin/deploy";
      };
    };
}
