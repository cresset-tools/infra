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
#   /srv/sync/monorepo/          colocated jj/Git working clone the worker reads
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
  workClone = "${syncDir}/monorepo";          # --repo-root: the jj/Git clone the worker reads
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
  # NOTE: the cresset-sync crate does not yet implement GitHub App auth
  # (JWT -> installation token -> git credential); these are wired ahead of that
  # code. The read-only Milestone-1 rollout does not USE them at runtime, but
  # sops-nix still decrypts them at activation.
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
      install -d -o ${user} -g ${user} -m 0750 ${syncDir} ${syncDir}/mirrors
      # The colocated jj/Git working clone the worker reads (--repo-root). It is
      # SEEDED ONCE by the operator after the canonical repo has content, e.g.:
      #   sudo -u ${user} jj git clone ${canonicalRepo} ${workClone}
      # (kept a manual bootstrap step — a fresh canonical repo is empty, so an
      # automatic clone here would fail on day zero). Just ensure the parent is
      # writable by the worker.
      install -d -o ${user} -g ${user} -m 0750 ${workClone}
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
  # without touching them. Once the read-only rollout is verified, an operator
  # enables mutation by appending `--apply --export-project <id>` to ExecStart
  # (Milestone 2 is gated to a single low-risk repo; `--apply` without the required
  # `--export-project` gating refuses, exactly like a non-dry-run `reconcile`).
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
      # `--apply --export-project <id>`. `--canonical-repo` is already wired so the
      # import phase has the local canonical repo to advance the moment `--apply`
      # lands; it is unused by the read-only pass.
      ExecStart = ''
        ${cresset-sync}/bin/cresset-sync \
          --repo-root ${workClone} \
          --db ${stateDb} \
          --canonical-repo ${canonicalRepo} \
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
