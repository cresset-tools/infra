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
            [
              (hostDir + "/configuration.nix")
            ]
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
          updateHook = pkgs.writeShellApplication {
            name = "update";
            runtimeInputs = [ pkgs.git pkgs.util-linux ];
            text = builtins.readFile ./hosts/internal/hooks/update;
          };
        in
        pkgs.runCommand "canonical-review-hooks" {
          nativeBuildInputs = [ pkgs.git pkgs.jujutsu ];
        } ''
          set -eu
          export HOME=$TMPDIR JJ_CONFIG=/dev/null
          cd "$TMPDIR"

          # --shared=group, exactly as hosts/internal/git-canonical.nix creates it. Not
          # cosmetic: --shared sets receive.denyNonFastForwards = true as a side effect, which
          # refuses every re-push of an amended review bookmark. A plain `git init --bare` here
          # meant the fixture was missing the one setting that broke the real repository, and
          # the checks passed while the box refused the core review loop.
          git init -q --bare -b main canonical.git
          install -m 0755 ${hook}/bin/post-receive canonical.git/hooks/post-receive
          install -m 0755 ${updateHook}/bin/update canonical.git/hooks/update
          # Where the host points these too, only inside the sandbox. Config rather than
          # environment for the same reason as in production: a pusher cannot set it.
          git --git-dir=canonical.git config cresset.approvalsFile "$TMPDIR/approved"
          # Somewhere the sandbox can see the sentinels land. Production uses the default,
          # /srv/git, which no test can write to -- so before this knob existed the only thing
          # checked about the signalling was that the hook did not crash on the way past it.
          mkdir -p "$TMPDIR/sentinels"
          git --git-dir=canonical.git config cresset.sentinelDir "$TMPDIR/sentinels"
          git --git-dir=canonical.git config cresset.breakGlassFile "$TMPDIR/break-glass"
          # One ungated subtree, as hosts/internal/git-canonical.nix configures the real
          # repository. Checks 15-19 are about this one line.
          git --git-dir=canonical.git config --add cresset.ungatedPath experiments
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

          # 6b. Deleting a landed review bookmark is ordinary cleanup and must be quiet.
          #
          #     The new oid is all zeros, and `git rev-list` on that fails with `fatal: bad
          #     object`; under `set -eu` that aborts the hook, so a push deleting one bookmark
          #     and updating another would skip the update. The pinned patch sets must survive
          #     the bookmark, which is the whole reason they are separate refs.
          cd "$TMPDIR/work"
          before=$(g for-each-ref 'refs/changes/**' | wc -l)
          jj bookmark delete review/thing >/dev/null
          jj git push --deleted >/dev/null 2>del.txt \
            || { echo "FAIL: deleting a review bookmark was rejected"; cat del.txt; exit 1; }
          grep -q "bad object" del.txt \
            && { echo "FAIL: the hook errored on a deletion"; cat del.txt; exit 1; }
          test "$(g for-each-ref 'refs/changes/**' | wc -l)" = "$before" \
            || { echo "FAIL: deleting the bookmark lost its patch sets"; exit 1; }
          test -z "$(g for-each-ref 'refs/heads/review/thing')" \
            || { echo "FAIL: the bookmark was not deleted"; exit 1; }

          # 6c. Amending a change under review and re-pushing must work.
          #
          #     This is the core review loop -- a second round of review is a rewrite by
          #     construction, because jj gives the amended change a new commit id. It failed on
          #     the real repository for months while this check passed, because the fixture was
          #     built without --shared=group and therefore without the denyNonFastForwards that
          #     --shared silently sets.
          cd "$TMPDIR/work"
          #     Applied HERE rather than at setup. The host gets it from `git init --shared=group`,
          #     which sets it as a side effect; the sandbox cannot use --shared (no group to make
          #     files writable by, and core.sharedRepository alone makes git chmod and fail the
          #     same way), so the one setting that matters is reproduced directly. It cannot be
          #     set globally in this fixture because it breaks checks 2 and 4 by design — they
          #     amend and re-push, which is precisely what it refuses.
          g config receive.denyNonFastForwards true
          jj new main -m "to be amended" >/dev/null
          echo first > amend.txt
          jj bookmark set review/amended -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/amended >/dev/null 2>&1
          jj edit 'review/amended' >/dev/null
          echo second > amend.txt
          jj bookmark set review/amended -r @ >/dev/null
          jj new >/dev/null
          if jj git push -b review/amended >/dev/null 2>push.txt; then
            echo "FAIL: the fixture is not reproducing denyNonFastForwards; this proves nothing"
            exit 1
          fi
          grep -q "denying non-fast-forward" push.txt \
            || { echo "FAIL: expected the remote to refuse the rewrite"; cat push.txt; exit 1; }

          #     Now the remedy the host applies, and the same push must succeed. Both directions
          #     are asserted, so this cannot pass by the disease simply being absent.
          g config receive.denyNonFastForwards false
          jj git push -b review/amended >/dev/null 2>push.txt \
            || { echo "FAIL: re-pushing an amended review bookmark was refused"; cat push.txt; exit 1; }
          test "$(g show refs/heads/review/amended:amend.txt)" = second \
            || { echo "FAIL: the amended content did not reach the remote"; exit 1; }

          #     And the host really does apply it. This check builds its own repository, so
          #     without this it would keep passing if the line were dropped from the module that
          #     configures the real one.
          grep -q "receive.denyNonFastForwards false" ${./hosts/internal/git-canonical.nix} \
            || { echo "FAIL: git-canonical.nix no longer disables denyNonFastForwards"; exit 1; }

          # --- the approval gate ------------------------------------------------------------
          #
          # From here on, landing on main requires an approval. These checks are the contract
          # between cresset-view and the hook; if they drift, either nothing can be pushed or
          # unreviewed code reaches 31 GitHub repositories.
          #
          # Each change is pushed for review BEFORE being landed, which is both the real
          # workflow and the thing that fixes its commit id: a reviewer approves the id they
          # were shown, and that id came from a pushed patch set. Staging straight onto main
          # from the working copy gives an id that changes again at push time -- which is how
          # this check first failed, and worth knowing before debugging a refused push.

          cd "$TMPDIR/work"

          # Put a change on a review bookmark and leave @ off it, then answer with its ids.
          submit() {
            jj new main -m "$1" >/dev/null
            printf '%s\n' "$3" > "$2"
            jj bookmark set "review/$4" -r @ >/dev/null
            jj new >/dev/null
            jj git push -b "review/$4" >/dev/null 2>&1
          }

          # What a commit DOES, which is what an approval is keyed to. Same two commands the
          # hook runs and cresset-view runs, deliberately -- see tools/cresset-view/src/patch_id.rs.
          pidof() {
            g diff-tree -p --no-commit-id "$1" | git patch-id --stable | cut -d' ' -f1
          }

          submit "work to land" h.txt landed land
          land=$(jj log -r 'review/land' --no-graph -T 'commit_id')
          landCid=$(jj log -r 'review/land' --no-graph -T 'change_id')
          jj bookmark set main -r 'review/land' >/dev/null

          # 7. Fail closed. There is no approvals file at all yet, and that must refuse rather
          #    than wave the push through -- a gate that opens when its input is missing is not
          #    a gate.
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: a push landed with no approvals file"; cat push.txt; exit 1
          fi
          grep -q "could not be read" push.txt \
            || { echo "FAIL: the refusal must say the file was unreadable"; cat push.txt; exit 1; }

          # 8. An empty approvals file is still a refusal, and names what to approve and where.
          : > "$TMPDIR/approved"
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: a push landed with nothing approved"; cat push.txt; exit 1
          fi
          grep -q "$landCid" push.txt \
            || { echo "FAIL: the refusal must name the unapproved change"; cat push.txt; exit 1; }
          grep -q "code.cresset.tools/?change=$landCid" push.txt \
            || { echo "FAIL: the refusal must say where to review it"; cat push.txt; exit 1; }

          # 9. Approving the exact DIFF lets it through. This is the format cresset-view
          #    writes; tools/cresset-view/src/approvals.rs asserts the other side of it.
          echo "$landCid $(pidof "$land")" > "$TMPDIR/approved"
          jj git push -b main >/dev/null 2>push.txt \
            || { echo "FAIL: an approved push was refused"; cat push.txt; exit 1; }
          test "$(g rev-parse refs/heads/main)" = "$land" \
            || { echo "FAIL: main did not advance to the approved commit"; exit 1; }

          # 10. Approval is keyed to the DIFF, not the change. Amending after approving means
          #     nobody has read what would land, so the stale line must not satisfy the gate.
          #     This is the half that keeps the gate a gate: without it the change id alone
          #     would do, and an amend after approval would land unread.
          submit "second" i.txt more second
          secondCid=$(jj log -r 'review/second' --no-graph -T 'change_id')
          echo "$secondCid $(pidof "$(jj log -r 'review/second' --no-graph -T 'commit_id')")" \
            > "$TMPDIR/approved"
          jj edit 'review/second' >/dev/null
          echo more-amended > i.txt
          jj bookmark set review/second -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/second >/dev/null 2>&1
          jj bookmark set main -r 'review/second' >/dev/null
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: an approval of an earlier patch set satisfied the gate"; cat push.txt; exit 1
          fi

          # 11. Break-glass: one push, then it is gone, and it says so loudly.
          touch "$TMPDIR/break-glass"
          jj git push -b main >/dev/null 2>push.txt \
            || { echo "FAIL: break-glass did not let the push through"; cat push.txt; exit 1; }
          grep -q "BREAK-GLASS" push.txt \
            || { echo "FAIL: break-glass must announce itself"; cat push.txt; exit 1; }
          test ! -e "$TMPDIR/break-glass" \
            || { echo "FAIL: break-glass was not consumed"; exit 1; }

          # 12. And it really was one-shot.
          submit "third" j.txt third third
          jj bookmark set main -r 'review/third' >/dev/null
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: break-glass allowed a second push"; cat push.txt; exit 1
          fi

          # 12b. A REBASE KEEPS ITS APPROVAL. The case this gate was rebuilt for.
          #
          #      Jelle approved a change, main moved by something unrelated, and rebasing threw
          #      the approval away -- so he was asked to re-read text he had already read. Since
          #      cresset-view's own Rebase button produces exactly this, it was the common case.
          #      Keyed to the commit id the gate could not tell a rebase from an edit; keyed to
          #      the patch id it can, and check 10 above is the other half of the proof.
          #      Start from the remote's main, not the local bookmark: checks 11 and 12 left it
          #      pointing at a commit the remote refused, and building on that would push an
          #      unapproved commit alongside this one and prove nothing about rebasing.
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null
          jj new 'main@canonical' -m "approved then rebased" >/dev/null
          echo body > m.txt
          jj bookmark set review/rebased -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/rebased >/dev/null 2>&1
          rebasedCid=$(jj log -r 'review/rebased' --no-graph -T 'change_id')
          echo "$rebasedCid $(pidof "$(jj log -r 'review/rebased' --no-graph -T 'commit_id')")" \
            > "$TMPDIR/approved"

          #      Something unrelated lands on main underneath it.
          jj new 'main@canonical' -m "unrelated, landed meanwhile" >/dev/null
          echo meanwhile > unrelated.txt
          jj bookmark set review/unrelated -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/unrelated >/dev/null 2>&1
          unrelatedCid=$(jj log -r 'review/unrelated' --no-graph -T 'change_id')
          echo "$unrelatedCid $(pidof "$(jj log -r 'review/unrelated' --no-graph -T 'commit_id')")" \
            >> "$TMPDIR/approved"
          jj bookmark set main -r 'review/unrelated' >/dev/null
          jj git push -b main >/dev/null 2>push.txt \
            || { echo "FAIL: the unrelated change would not land"; cat push.txt; exit 1; }

          #      Now rebase the approved change onto the new main. Every commit id changes.
          wasAt=$(jj log -r 'review/rebased' --no-graph -T 'commit_id')
          jj rebase -b 'review/rebased' -d main >/dev/null 2>&1
          nowAt=$(jj log -r 'review/rebased' --no-graph -T 'commit_id')
          test "$wasAt" != "$nowAt" \
            || { echo "FAIL: the fixture did not actually rebase; this proves nothing"; exit 1; }
          jj git push -b review/rebased >/dev/null 2>&1

          #      The approval was never rewritten, and the push must still be allowed.
          jj bookmark set main -r 'review/rebased' >/dev/null
          jj git push -b main >/dev/null 2>push.txt \
            || { echo "FAIL: a rebased but approved change was refused"; cat push.txt; exit 1; }
          test "$(g rev-parse refs/heads/main)" = "$nowAt" \
            || { echo "FAIL: main did not advance to the rebased commit"; exit 1; }

          # 12c. A merge commit is refused by name rather than read as an empty diff.
          #
          #      `git diff-tree -p` prints nothing for a merge without -m, so without the guard
          #      it takes the empty-diff path -- and an approval of an empty change would then
          #      satisfy a merge carrying anything at all.
          #
          #      Built with jj, because a merge made with plain git has no change-id and is
          #      refused for THAT instead, several branches earlier. The first version of this
          #      check did exactly that and asserted nothing about merges.
          jj new 'main@canonical' -m "left" >/dev/null
          echo left > left.txt
          left=$(jj log -r @ --no-graph -T 'commit_id')
          leftCid=$(jj log -r @ --no-graph -T 'change_id')
          jj new 'main@canonical' -m "right" >/dev/null
          echo right > right.txt
          right=$(jj log -r @ --no-graph -T 'commit_id')
          rightCid=$(jj log -r @ --no-graph -T 'change_id')
          jj new "$left" "$right" -m "a merge" >/dev/null
          jj bookmark set review/merged -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/merged >/dev/null 2>&1

          #      Both sides approved, so the merge itself is the only thing left to object to.
          echo "$leftCid $(pidof "$left")" > "$TMPDIR/approved"
          echo "$rightCid $(pidof "$right")" >> "$TMPDIR/approved"
          jj bookmark set main -r 'review/merged' >/dev/null
          if jj git push -b main >/dev/null 2>merge.txt; then
            echo "FAIL: a merge commit landed on main"; cat merge.txt; exit 1
          fi
          grep -q "merge commit" merge.txt \
            || { echo "FAIL: the refusal must name the merge"; cat merge.txt; exit 1; }
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null

          # 13. THE PROPERTY MOST LIKELY TO BE BROKEN LATER. The sync worker advances main with
          #     a local `git update-ref`, which does not run hooks. That is not an oversight to
          #     be tidied up: imported work was reviewed on GitHub, and gating it here would
          #     stop synchronization dead. If someone ever "fixes" the inconsistency by making
          #     the worker push over SSH, this check is what says no.
          before=$(g rev-parse refs/heads/main)
          worker=$(jj log -r 'review/third' --no-graph -T 'commit_id')
          g update-ref refs/heads/main "$worker" "$before"
          test "$(g rev-parse refs/heads/main)" = "$worker" \
            || { echo "FAIL: a local update-ref must bypass the gate"; exit 1; }

          # 14. The gate is on main alone. Review bookmarks stay freely pushable, or the loop
          #     the gate exists to encourage would be the one thing it blocked. (`submit` has
          #     been pushing them unapproved all along; assert it rather than leaving it
          #     implied, so a gate widened to every ref fails here and not in someone's day.)
          submit "for review" k.txt r gated
          jj git push -b review/gated >/dev/null 2>push.txt \
            || { echo "FAIL: pushing for review must not need approval"; cat push.txt; exit 1; }

          # --- the ungated subtrees ---------------------------------------------------------
          #
          # `cresset.ungatedPath` lets a subtree land unreviewed. The risk is not that someone
          # commits a typo to it unread — that is the feature — but that the exemption turns
          # out wider than the prefix: a mixed commit riding in on it, a merge reading as
          # touching nothing, a sibling matching by string prefix, or a prefix that exempts
          # everything. Each of those is a check below.

          # Check 13 advanced the remote with a local `update-ref`, which jj never saw. Without
          # catching up, the first push of main from here fails with "stale info" and says
          # nothing at all about the gate.
          jj git fetch --remote canonical >/dev/null 2>&1
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null

          # 15. A commit confined to an ungated subtree lands with nothing approved, and says
          #     so. Nothing here is approved — the file still holds whatever check 12c left.
          mkdir -p experiments
          submit "an experiment" experiments/notes.md scratch exp
          jj bookmark set main -r 'review/exp' >/dev/null
          jj git push -b main >/dev/null 2>push.txt \
            || { echo "FAIL: an ungated commit was refused"; cat push.txt; exit 1; }
          test "$(g rev-parse refs/heads/main)" = "$(jj log -r 'review/exp' --no-graph -T 'commit_id')" \
            || { echo "FAIL: main did not advance to the ungated commit"; exit 1; }
          grep -q "landing without review" push.txt \
            || { echo "FAIL: landing unreviewed work must announce itself"; cat push.txt; exit 1; }

          # 16. A commit touching an ungated path AND anything else is an ordinary commit.
          #     The whole exemption in one check: what publishes is the commit, not the path,
          #     so a mixed commit must not ride in on its ungated half.
          jj new 'main@canonical' -m "an experiment and a product" >/dev/null
          mkdir -p experiments products/bougie
          echo more > experiments/notes.md
          echo sneak > products/bougie/leaked.txt
          jj bookmark set review/mixed -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/mixed >/dev/null 2>&1
          jj bookmark set main -r 'review/mixed' >/dev/null
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: a commit mixing an ungated path with a product path landed unreviewed"
            cat push.txt; exit 1
          fi
          grep -q "code.cresset.tools/?change=" push.txt \
            || { echo "FAIL: the mixed commit must be refused as unapproved"; cat push.txt; exit 1; }
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null

          # 17. The prefix is a directory boundary, not a string prefix. `experimentsx/` is a
          #     different subtree and still needs review.
          mkdir -p experimentsx
          submit "a lookalike subtree" experimentsx/notes.md scratch lookalike
          jj bookmark set main -r 'review/lookalike' >/dev/null
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: experimentsx/ was exempted by the experiments prefix"; cat push.txt; exit 1
          fi
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null

          # 18. A merge of two ungated commits is still refused. The hook refuses merges by
          #     name before it ever asks whether the paths are ungated, and that order is what
          #     stops a merge — which prints no diff, so no paths — from reading as exempt.
          jj new 'main@canonical' -m "experiment left" >/dev/null
          mkdir -p experiments
          echo l > experiments/l.txt
          expLeft=$(jj log -r @ --no-graph -T 'commit_id')
          jj new 'main@canonical' -m "experiment right" >/dev/null
          mkdir -p experiments
          echo r > experiments/r.txt
          expRight=$(jj log -r @ --no-graph -T 'commit_id')
          jj new "$expLeft" "$expRight" -m "merge two experiments" >/dev/null
          jj bookmark set review/expmerge -r @ >/dev/null
          jj new >/dev/null
          jj git push -b review/expmerge >/dev/null 2>&1
          jj bookmark set main -r 'review/expmerge' >/dev/null
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: a merge of ungated commits landed"; cat push.txt; exit 1
          fi
          grep -q "merge commit" push.txt \
            || { echo "FAIL: the refusal must name the merge"; cat push.txt; exit 1; }
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null

          # 19. A prefix that would exempt the whole repository STOPS the gate rather than
          #     opening it, and says which value it refused.
          #
          #     The commit pushed here is one that the good prefix would exempt, so the only
          #     thing that can refuse it is the configuration check itself — and the refusal
          #     must come from there rather than from the gate further down. Checking only
          #     that the push failed passes even if the hook merely complains and carries on,
          #     which is how the first version of this check proved nothing.
          g config --add cresset.ungatedPath /
          mkdir -p experiments
          submit "an experiment under a bad config" experiments/again.md scratch badcfg
          jj bookmark set main -r 'review/badcfg' >/dev/null
          if jj git push -b main >/dev/null 2>push.txt; then
            echo "FAIL: an absolute ungatedPath did not stop the gate"; cat push.txt; exit 1
          fi
          grep -q "not a repository-relative" push.txt \
            || { echo "FAIL: the refusal must name the bad configuration"; cat push.txt; exit 1; }
          grep -q "not every commit has been approved" push.txt \
            && { echo "FAIL: the hook complained about the config and then ran the gate anyway"; cat push.txt; exit 1; }
          g config --unset-all cresset.ungatedPath
          g config --add cresset.ungatedPath experiments
          jj bookmark set main -r 'main@canonical' --allow-backwards >/dev/null

          # 20. And the host really configures one. This check builds its own repository, so
          #     without this the exemption could be dropped from the module that configures the
          #     real one and every check above would still pass.
          grep -q "cresset.ungatedPath" ${./hosts/internal/git-canonical.nix} \
            || { echo "FAIL: git-canonical.nix no longer writes cresset.ungatedPath"; exit 1; }
          #     And that the list it writes is not empty, which the grep above cannot tell:
          #     the config line and its comments survive `ungatedPaths = [ ]` intact, so for
          #     one revision this check passed while the host exempted nothing.
          grep -qE 'ungatedPaths = \[ *"' ${./hosts/internal/git-canonical.nix} \
            || { echo "FAIL: git-canonical.nix configures no ungated path at all"; exit 1; }

          # 21-24. The sentinels: which kind of push asks for which piece of work.
          #
          #     These are how the hook reaches the two services that talk to GitHub, and the
          #     mechanism fails SILENTLY -- a sentinel that is never touched looks exactly like
          #     a quiet repository. The cleanup signal in particular is the only notice anything
          #     takes of an abandoned change: once its bookmark is gone there is nothing left in
          #     the monorepo to find, so a push that does not raise it means the downstream
          #     branch and its pull request stay there for ever.
          sentinels="$TMPDIR/sentinels"
          clearSentinels() { rm -f "$sentinels"/.refresh-requested "$sentinels"/.submit-requested "$sentinels"/.cleanup-requested; }
          raised() { test -e "$sentinels/.$1-requested"; }

          # 21. Pushing a change for review asks for a submit, and NOT for a cleanup. Cleanup
          #     deletes branches; a signal on every patch set would point it at live work.
          clearSentinels
          submit "a change that will be abandoned" sentinel.txt gone sentinel
          raised refresh || { echo "FAIL: a review push did not wake the viewer"; exit 1; }
          raised submit || { echo "FAIL: a review push did not ask for a submit"; exit 1; }
          raised cleanup && { echo "FAIL: a review push asked for a cleanup"; exit 1; }

          # 22. Deleting a review bookmark asks for a cleanup, and not for a submit -- there is
          #     nothing left to submit, and this is the abandonment signal.
          clearSentinels
          jj bookmark delete review/sentinel >/dev/null
          jj git push --deleted >/dev/null 2>&1
          raised cleanup || { echo "FAIL: deleting a review bookmark did not ask for a cleanup"; exit 1; }
          raised submit && { echo "FAIL: a deletion asked for a submit with nothing to submit"; exit 1; }

          # 23. And the deletion did not disturb the patch sets. The commit it pinned is
          #     unreachable from any branch now, which is exactly why the pin is what keeps an
          #     abandoned change readable after the fact.
          sentinelCid=$(jj log -r 'present(review/sentinel)' --no-graph -T 'change_id' 2>/dev/null || true)
          test -z "$sentinelCid" || { echo "FAIL: the bookmark is still there"; exit 1; }

          # 24. Landing on main asks for a cleanup: the change is finished, so whatever it left
          #     in the downstream repositories is now stale. This is the common case -- most
          #     changes land rather than being abandoned -- and `main` is not a `review/*` ref,
          #     so it is reached by its own arm of the hook's dispatch.
          clearSentinels
          submit "a change that will land" sentinel2.txt landing sentinel2
          clearSentinels
          landed2=$(jj log -r 'review/sentinel2' --no-graph -T 'commit_id')
          echo "$(jj log -r 'review/sentinel2' --no-graph -T 'change_id') $(pidof "$landed2")" > "$TMPDIR/approved"
          jj bookmark set main -r 'review/sentinel2' >/dev/null
          jj git push -b main >/dev/null 2>push.txt \
            || { echo "FAIL: an approved push was refused"; cat push.txt; exit 1; }
          raised cleanup || { echo "FAIL: landing on main did not ask for a cleanup"; exit 1; }
          raised refresh || { echo "FAIL: landing on main did not wake the viewer"; exit 1; }

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
            # Exposed as a package, not only as part of the host, because the CLI half runs on
            # a developer's machine: `nix run <this flake>#cresset-review -- threads <id>`.
            cresset-review = import ./tools/cresset-review/package.nix { inherit pkgs; };
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
