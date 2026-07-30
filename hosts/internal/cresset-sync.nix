# The cresset-sync worker on `internal`.
#
# Reuses the fleet's proven "long-lived worker + durable SQLite + systemd timer"
# shape (Resolved Design Question 5), the same one hosts/origin/mageos-maker.nix
# runs. The worker keeps the monorepo `main` (the LOCAL canonical bare repo in
# git-canonical.nix) converged with the 30 cresset-tools/* GitHub default
# branches.
#
# `internal` is co-located with the existing Authentik + PostgreSQL + cresset-view
# stack (configuration.nix). It carries a dedicated /srv Hetzner Cloud Volume
# (disko.nix), so all durable working data lives under /srv — off the read-only
# Nix store and off the root disk:
#
#   /srv/git/cresset.git         canonical bare repo (git-canonical.nix) — advanced LOCALLY
#   /srv/sync/state.db           SQLite checkpoint DB (authoritative)
#   /srv/sync/mirrors/           per-repo bare Git mirrors of the downstreams
#
# There is exactly ONE copy of the monorepo here — the canonical bare repo above is
# also what the worker reads and advances. It shells out only to `git`, so it never
# needed a jj repo or a working copy of its own.
#
# A `git` pack/repack spike is the realistic OOM risk on a shared box, so this
# module also lands memory GUARDRAILS: zram swap PLUS a disk swapfile on the /srv
# volume (this 4 GB box is shared with Authentik + PostgreSQL, so real overflow
# headroom matters — the box stays 4 GB, no resize), a MemoryHigh/MemoryMax cap
# on the worker cgroup (which contains its git/jj children), and bounded git pack
# tuning in /etc/gitconfig.
{ config, pkgs, lib, inputs, ... }:
let
  user = "cresset-sync";
  syncDir = "/srv/sync";
  stateDb = "${syncDir}/state.db";            # mirrors dir is derived as <db-parent>/mirrors
  # ONE copy of the monorepo on this host. The worker reads and advances the canonical bare
  # repository directly as its --repo-root: it only ever spawns `git`, so it never needed a jj
  # repo or a working copy, and `.sync/projects.toml` is read out of the commit being
  # reconciled rather than off disk. There is deliberately no separate working clone to seed,
  # keep in sync, or let go stale.
  canonicalRepo = "/srv/git/cresset.git";     # advanced locally (co-located, no SSH round-trip)

  # Consume the worker as a FlakeHub artifact, the SAME mechanism as
  # hosts/bougie-relay consumes cresset-tools/bougie-relay: the monorepo CI
  # publishes operations/sync as a private FlakeHub input (GitHub OIDC), and this
  # flake builds it here with buildRustPackage. Keeping `operations/sync`
  # monorepo-internal (no cresset-tools/* repo, Resolved Q7) while remaining
  # buildable from the standalone cresset-tools/infra clone.
  #
  # The worker SHELLS OUT to pinned git/jj (Resolved "jj library crate vs. CLI";
  # SYNC_WORKER.md security rules), so wrap the binary to put THIS build's git +
  # jujutsu on its PATH — the deployment package owns the pinned toolchain.
  cresset-sync = pkgs.rustPlatform.buildRustPackage {
    pname = "cresset-sync";
    version = "0.1.0";
    src = inputs.cresset-sync;
    cargoLock.lockFile = inputs.cresset-sync + "/Cargo.lock";
    # rusqlite is `features = ["bundled"]`, so it compiles a vendored SQLite —
    # buildRustPackage's stdenv cc covers it; no system sqlite needed.
    nativeBuildInputs = [ pkgs.makeWrapper ];
    # buildRustPackage runs the test suite in its check phase, and the fixture tests
    # build real jj/Git repositories on disk — so the SAME pinned toolchain the wrapper
    # puts on the runtime PATH has to be present at BUILD time too, or the build dies
    # with "failed to spawn jj". The tests are hermetic (local temp repos, a throwaway
    # JJ_CONFIG, explicit Git identities, no network), so they run happily in the
    # sandbox once the binaries exist. The github_app_live.rs checks are #[ignore]d and
    # stay skipped here — they need real credentials and deliberately hit GitHub.
    nativeCheckInputs = [ pkgs.git pkgs.jujutsu ];
    postInstall = ''
      wrapProgram $out/bin/cresset-sync \
        --prefix PATH : ${lib.makeBinPath [ pkgs.git pkgs.jujutsu ]}
    '';
  };
in
{
  # ---- Worker service account ----
  # A fixed system user (not DynamicUser: it must be a stable owner of the
  # /srv state across redeploys, and a member of the `git` group so it can
  # advance the local canonical repo's refs). Home is the sync dir.
  users.users.${user} = {
    isSystemUser = true;
    group = user;
    home = syncDir;
    # Advancing `main` on /srv/git/cresset.git locally means writing
    # refs/objects in the (group-shared) bare repo; membership in `git` grants that.
    extraGroups = [ "git" ];
  };
  users.groups.${user} = { };

  # ---- GitHub App credentials (sops-nix) ----
  # Read/write Contents on the 30 downstream repos. Declared here (the module
  # that consumes them); the host-wide sops defaults (defaultSopsFile,
  # sshKeyPaths) live in configuration.nix. Decrypted to /run/secrets at
  # activation with the box's own SSH host key. NOTE: no monorepo-advance SSH
  # key — the canonical `main` advance is LOCAL (supersedes Phase 7's SSH
  # sub-point), so the App is the only credential this worker needs.
  #
  # The GitHub App is the only credential this worker needs. secrets/internal.yaml
  # carries `github_app/client_id` + `github_app/private_key` (the App PEM),
  # encrypted to the admin + host_internal age recipients. GitHub accepts the
  # client_id as the JWT issuer; the installation id is discovered at runtime
  # (GET /app/installations) rather than stored.
  #
  # These credentials are live and verified end-to-end against the real API by
  # operations/sync/tests/github_app_live.rs (run it with `--ignored`): the App mints
  # an installation token good for 60 min, the installation reaches all 30 mapped
  # repositories, and a `git push --dry-run` against cresset-tools/wick is accepted,
  # so Contents: write is really granted.
  #
  # The git credential is HTTP **basic** auth with the `x-access-token` username, not
  # Bearer: GitHub's REST API accepts Bearer but its *git* endpoints reject it with
  # `remote: invalid credentials` — including on public repos, where a bad credential
  # is refused outright instead of falling back to anonymous access. See
  # repo.rs's GitCredential.
  #
  # The private key is a file secret (owned by the worker so it can read it); the
  # client id is rendered into an EnvironmentFile alongside a pointer to that file.
  sops.secrets."github_app/private_key" = { owner = user; };
  sops.templates."cresset-sync.env" = {
    owner = user;
    content = ''
      GITHUB_APP_CLIENT_ID=${config.sops.placeholder."github_app/client_id"}
      GITHUB_APP_PRIVATE_KEY_FILE=${config.sops.secrets."github_app/private_key".path}
    '';
  };
  # client_id is a non-file scalar pulled in only for the template placeholder above.
  sops.secrets."github_app/client_id" = { owner = user; };

  # ---- /srv working-data provisioning ----
  # /srv is the Hetzner Cloud Volume, mounted with `nofail` (so it is excluded
  # from local-fs.target's requires set). Exactly like origin's /srv-dependent
  # services, gate this explicitly on the `srv.mount` unit so it never writes
  # into an unmounted /srv, and hold it before the worker starts.
  systemd.services.cresset-sync-setup = {
    description = "Provision cresset-sync working dirs under /srv";
    wantedBy = [ "multi-user.target" ];
    after = [ "srv.mount" ];
    requires = [ "srv.mount" ];
    before = [ "cresset-sync.service" ];
    path = [ pkgs.coreutils ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      RequiresMountsFor = "/srv";
    };
    script = ''
      set -euo pipefail
      # Only the worker's own state lives here: the checkpoint DB, the per-repo
      # downstream mirrors, and the lease lock. The monorepo itself is the canonical
      # bare repo (git-canonical.nix) — there is no working clone to provision, which
      # also means no manual seeding step before the first pass can run.
      install -d -o ${user} -g ${user} -m 0750 ${syncDir} ${syncDir}/mirrors
    '';
  };

  # ---- The worker (timer-driven reconciliation pass) ----
  # Same shape as mageos-maker.nix's catalog oneshot+timer (mageos-maker.nix
  # :273-296): a oneshot service performing one reconciliation pass, fired every
  # 5 minutes by the timer below. `run` is the worker's production entrypoint;
  # it performs ONE reconciliation pass and exits, and the periodic cadence is the
  # timer (matches the design's "periodic reconciliation loop; webhooks are only
  # latency hints").
  #
  # READ-ONLY MILESTONE-1 ROLLOUT: bare `run` (below) performs NO mutation — no
  # pushes, no `main` advances — exactly like `reconcile --dry-run`, exiting 0. It
  # exercises the checkpoint + tree-equality core against the live downstreams
  # without touching them.
  #
  # Enabling mutation is TWO independent steps, deliberately: append `--apply` to
  # ExecStart, and turn projects on one at a time with
  # `cresset-sync enable <project>`. The enable switch is durable state, defaults to
  # off for every project, and is never overwritten by a checkpoint write — so
  # `--apply` on its own synchronises nothing and says so, rather than starting on
  # all thirty-one repositories at once. `cresset-sync disable <project>` is the
  # emergency per-project pause. (Legacy note: `--apply --export-project <id>`
  # still restricts a pass to one project regardless of its switch, for targeted
  # operator runs.)
  systemd.services.cresset-sync = {
    description = "cresset-sync: converge monorepo main with cresset-tools/* GitHub repos";
    # No wantedBy = multi-user.target — the timer owns activation (like the
    # mageos-maker-catalog oneshot). after/requires ensure state + repo exist.
    # network.target matches the fleet convention (bougie-collector, mageos-maker);
    # a pass that fires before the network is up simply fails and the timer retries
    # in 5 minutes.
    after = [ "network.target" "srv.mount" "cresset-sync-setup.service" "cresset-git-canonical-init.service" ];
    requires = [ "cresset-sync-setup.service" "cresset-git-canonical-init.service" ];
    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = user;
      # All working data + the canonical repo live on the /srv Cloud Volume
      # (nofail mount). Hold the pass until /srv is actually mounted so it never
      # runs against an unmounted volume.
      RequiresMountsFor = "/srv";
      # Secret env-file (sops): the GitHub App id/installation/private-key
      # pointer. Optional (`-`) so the read-only Milestone-1 service still starts
      # before the real App creds are provisioned.
      EnvironmentFile = [ "-${config.sops.templates."cresset-sync.env".path}" ];
      # Read-only Milestone-1: bare `run`, NO `--apply` (see the block above). To
      # enable mutation once the read-only rollout is verified, append
      # `--apply --export-project <id>`.
      #
      # `--repo-root` is the canonical bare repo itself, and `--canonical-repo` is
      # deliberately absent: it defaults to the same store, which is the point — the
      # import advances the very ref the export then plans against, so there is no
      # window in which the two can disagree.
      ExecStart = ''
        ${cresset-sync}/bin/cresset-sync \
          --repo-root ${canonicalRepo} \
          --db ${stateDb} \
          run
      '';
      # ---- GUARDRAIL: bound the worker cgroup ----
      # A git pack/repack of a Magento-sized snapshot is the OOM risk. This cap
      # covers the worker AND its git/jj children (same cgroup); combined with
      # the bounded pack tuning below and zram swap, a spike is throttled/killed
      # inside the unit rather than OOM-killing the box (which also runs Authentik
      # + PostgreSQL).
      MemoryHigh = "1500M";
      MemoryMax = "2G";
      # Hardening — the worker only needs its /srv state + the local canonical
      # repo writable, and to reach git/jj + GitHub.
      NoNewPrivileges = true;
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
      # /srv/sync holds all writable working data (state, mirrors, clone) and
      # /srv/git/cresset.git is the canonical repo the worker advances locally.
      ReadWritePaths = [ syncDir canonicalRepo ];
      ProtectKernelTunables = true;
      ProtectKernelModules = true;
      ProtectControlGroups = true;
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
      RestrictNamespaces = true;
      LockPersonality = true;
      SystemCallArchitectures = "native";
      SystemCallFilter = [ "@system-service" "~@privileged" ];
    };
  };

  # ---- Dead-man's switch ----
  # The reconcile unit going red only catches a pass that RAN and failed. It cannot catch the
  # timer being masked, the box being wedged, or /srv being unmounted — and because one blocked
  # project pauses every project, a silent stall is indistinguishable from convergence.
  #
  # `health` reads durable state only (no remotes, no lease) and exits non-zero when no pass has
  # completed recently or any project is blocked. Running it on its own timer means the failure
  # shows up in `systemctl --failed` and the journal without needing the worker to be alive to
  # report it. Choosing a push channel — mail, ntfy, an external check-in — is a fleet-wide
  # decision, so this deliberately stops at "the unit is red", which is at least discoverable.
  systemd.services.cresset-sync-health = {
    description = "cresset-sync liveness/health check (durable state only)";
    after = [ "srv.mount" ];
    requires = [ "cresset-sync-setup.service" ];
    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = user;
      RequiresMountsFor = "/srv";
      # Threshold is generous relative to the 5-minute reconcile cadence: several consecutive
      # missed or failing passes, not a single hiccup, is what should page anyone.
      ExecStart = ''
        ${cresset-sync}/bin/cresset-sync \
          --repo-root ${canonicalRepo} \
          --db ${stateDb} \
          health --max-age-secs 1800
      '';
      NoNewPrivileges = true;
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
      # Read-only: it inspects state, it never advances anything. The SQLite WAL still needs
      # write access to the database directory, so this is not ReadOnlyPaths.
      ReadWritePaths = [ syncDir ];
      ProtectKernelTunables = true;
      ProtectKernelModules = true;
      ProtectControlGroups = true;
      RestrictAddressFamilies = [ "AF_UNIX" ];
      RestrictNamespaces = true;
      LockPersonality = true;
      SystemCallArchitectures = "native";
      SystemCallFilter = [ "@system-service" "~@privileged" ];
    };
  };

  systemd.timers.cresset-sync-health = {
    description = "Check cresset-sync liveness every 15 minutes";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "*:0/15";
      Persistent = true;
      RandomizedDelaySec = "60s";
    };
  };

  systemd.timers.cresset-sync = {
    description = "Drive the cresset-sync reconciliation loop every 5 minutes";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "*:0/5";
      Persistent = true;    # catch up one pass if the box was off at a fire
      RandomizedDelaySec = "30s";
    };
  };

  # ---- GUARDRAIL: swap ----
  # Two tiers, because this 4 GB box is shared with Authentik + PostgreSQL and
  # the box stays 4 GB (no resize):
  #   - zram gives compressed in-RAM swap (fast, always present) that absorbs a
  #     transient git pack spike without touching disk;
  #   - a disk swapfile on the /srv Cloud Volume gives real overflow headroom
  #     beyond RAM+zram, so a larger pack/repack pages to disk instead of
  #     OOM-killing the box. It lives on /srv (the volume) so it does not consume
  #     the small root disk.
  zramSwap = {
    enable = true;
    memoryPercent = 50;
  };
  swapDevices = [
    {
      device = "/srv/swapfile";
      size = 4096; # MiB
    }
  ];

  # ---- GUARDRAIL: bounded git pack tuning (system-wide) ----
  # Applies to BOTH the worker's git invocations and receive-pack on the
  # canonical repo, so neither can spin up an unbounded multi-threaded repack
  # that blows the memory budget. Single pack thread + capped window/delta memory
  # + a low big-file threshold (Magento trees carry large generated assets).
  environment.etc."gitconfig".text = ''
    [pack]
        threads = 1
        windowMemory = 128m
        packSizeLimit = 128m
        deltaCacheSize = 64m
    [core]
        bigFileThreshold = 16m
  '';
}
