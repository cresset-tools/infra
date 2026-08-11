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
#   /srv/sync/jj-workspace/      jj metadata only — attached to the canonical store
#
# There is exactly ONE copy of the monorepo here. The canonical bare repo above is
# also what the worker reads and advances, and the jj workspace it merges in is
# ATTACHED to that same object store rather than cloning it: `jj git init
# --git-repo=` plus `--ignore-working-copy` on every command means jj never checks
# anything out, so the workspace holds `.jj` bookkeeping and nothing else. A
# materialized Magento-sized tree there would have silently undone that.
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
  # jujutsu on its PATH — the deployment package owns the pinned toolchain. `jj` is a
  # genuine runtime dependency now, not just a build-time one: imports replay through
  # `jj rebase` so that conflicts are recorded structurally rather than as marker text.
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

  # cresset-view reads the checkpoint database to show which projects are blocked and how stale
  # the fleet is — the panel the Telegram escalation's link lands beside. Group membership is
  # what grants that; the viewer opens the database read-only and can never write it.
  users.users.cresset-view.extraGroups = [ user ];

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
      TELEGRAM_BOT_TOKEN=${config.sops.placeholder."telegram/bot_token"}
      TELEGRAM_CHAT_ID=${config.sops.placeholder."telegram/chat_id"}
    '';
  };

  # ---- Operator notification (sops) ----
  # One Telegram message when a conflict genuinely needs a human, and the same channel for the
  # liveness check — a dead-man's switch and a conflict escalation should not be two mechanisms.
  #
  # If these are absent the worker still blocks, records and reports the conflict; it just says
  # so on stderr instead of sending. That is a legitimate deployment, not a broken one, so a
  # missing token never fails a pass.
  #
  # The message itself is deliberately a pointer — project, operation, a COUNT of conflicted
  # paths, and a cresset-view link — never file contents or diffs. Telegram is a third party,
  # and this is the same reasoning that ruled out GitHub issues for conflict reporting.
  # Verified end to end against the real bot: `cresset-sync notify-test` sends a message
  # shaped exactly like a real escalation, so a wrong token or chat id surfaces immediately
  # rather than the first time something actually breaks.
  sops.secrets."telegram/bot_token" = { owner = user; };
  sops.secrets."telegram/chat_id" = { owner = user; };
  # client_id is a non-file scalar pulled in only for the template placeholder above.
  sops.secrets."github_app/client_id" = { owner = user; };

  # ---- Conflict-resolving agent ----
  # Automated resolution shells out to the Claude Code CLI, which authenticates as a
  # LOGGED-IN SESSION rather than an API key. Two consequences, both operational:
  #
  #   - the CLI IS packaged now, as pkgs.claude-code. An earlier note here said it was not
  #     in nixpkgs, which was true when this was written and is no longer. configuration.nix
  #     puts it on the system PATH, but it is still deliberately NOT wrapped onto the worker
  #     binary's PATH: the resolver runs only when `run` is given --agent-command, which
  #     ExecStart omits, so a missing or unconfigured agent degrades to "attempt failed →
  #     escalate to a human" instead of silently changing how conflicts are handled;
  #   - someone runs `claude auth login` ONCE per host as the ${user} user (the subcommand
  #     is `claude auth login`; a bare `claude login` is not a thing):
  #
  #       ssh -t root@internal.cresset.tools 'sudo -u ${user} -H claude auth login'
  #
  #     `-H` is the part that matters: it points HOME at ${syncDir}, which is where the
  #     worker looks. The account's nologin shell is not an obstacle, because sudo execs
  #     the command directly rather than through a login shell. Credentials land in
  #     $HOME/.claude/.credentials.json and are refreshed in place, so that directory must
  #     stay writable — it does, since HOME is ${syncDir} and ReadWritePaths covers it.
  #     `ProtectHome = true` looks like it would block this but does not: it hides /home,
  #     /root and /run/user, and this user's home is under /srv.
  #
  # A lapsed login surfaces as a failed attempt, which escalates exactly like any other
  # agent failure. That is the correct degradation — synchronization pauses and somebody is
  # told — but it is discovered at conflict time, not before.
  #
  # The agent runs with `--safe-mode`, which disables CLAUDE.md, hooks, plugins, MCP and
  # custom agents while leaving auth alone. That matters beyond determinism: a CLAUDE.md
  # arriving inside the very repository content being merged would otherwise steer the
  # resolver, which is prompt injection rather than a hypothesis.

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
    # RequiresMountsFor is a [Unit] directive. It sat in serviceConfig until systemd was
    # caught rejecting it -- "Unknown key 'RequiresMountsFor' in section [Service],
    # ignoring" -- so this guard was silently absent on every unit that declared it. The
    # /srv gating held anyway, on `requires = [ "srv.mount" ]`; this restores the second
    # layer it was always meant to have.
    unitConfig.RequiresMountsFor = "/srv";
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
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
  # ROLLOUT STATE: `--apply` is on, every mapped project is now enabled, and the
  # resolver still runs in suggest-only mode. Those are three independent switches on
  # purpose, and the third is deliberately still in its safe position.
  #
  # `--apply` opts into the state-mutating import+export path, but it synchronises
  # NOTHING on its own: `enable` is durable per-project state that defaults to off, so
  # the pass covers exactly the projects turned on with `cresset-sync enable <project>`
  # and says so when that set is empty. That is what made it safe to land this switch
  # before any project was enabled, rather than flipping both at once and starting on
  # all the repositories together — enablement then went out in small groups, per the
  # rollout in docs/SYNC_WORKER.md phase 17, until it covered all of them.
  # `cresset-sync disable <project>` is the emergency per-project pause.
  # (`--apply --export-project <id>` still restricts a pass to one project regardless
  # of its switch, for targeted operator runs.)
  #
  # `--agent-command` turns on automated conflict resolution. It is only meaningful
  # alongside `--apply`: the resolution policy is constructed solely on the `--apply`
  # branch, and a read-only pass never produces a conflict to resolve, so passing it
  # without `--apply` would be inert. The path is the absolute store path rather than
  # bare `claude`, pinning the exact build this deployment ships — the same reasoning
  # that pins git and jujutsu on the wrapper, and it keeps the resolver from silently
  # changing version underneath a running host.
  #
  # `--agent-apply` is deliberately ABSENT, which is what makes this suggest-only: an
  # accepted candidate is recorded and reported, never published. A bad resolution
  # reaching `main` is recoverable; the export that follows publishes it to a repository
  # the worker never force-pushes, and that is not. Add the flag once the recorded
  # candidates have been read and trusted.
  #
  # Attempts and timeout keep their defaults (2 attempts, 600s each). Every candidate
  # still goes through conflict::resolve, which applies exactly the checks a human's
  # resolution gets — conflict-free, tree-safe, inside its envelope — so a refusal is
  # the system working. Nothing the agent returns is taken on trust.
  systemd.services.cresset-sync = {
    description = "cresset-sync: converge monorepo main with cresset-tools/* GitHub repos";
    # No wantedBy = multi-user.target — the timer owns activation (like the
    # mageos-maker-catalog oneshot). after/requires ensure state + repo exist.
    # network.target matches the fleet convention (bougie-collector, mageos-maker);
    # a pass that fires before the network is up simply fails and the timer retries
    # in 5 minutes.
    after = [ "network.target" "srv.mount" "cresset-sync-setup.service" "cresset-git-canonical-init.service" ];
    requires = [ "cresset-sync-setup.service" "cresset-git-canonical-init.service" ];
    # RequiresMountsFor is a [Unit] directive. It sat in serviceConfig until systemd was
    # caught rejecting it -- "Unknown key 'RequiresMountsFor' in section [Service],
    # ignoring" -- so this guard was silently absent on every unit that declared it. The
    # /srv gating held anyway, on `requires = [ "srv.mount" ]`; this restores the second
    # layer it was always meant to have.
    unitConfig.RequiresMountsFor = "/srv";
    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = user;
      # All working data + the canonical repo live on the /srv Cloud Volume
      # (nofail mount). Hold the pass until /srv is actually mounted so it never
      # runs against an unmounted volume.
      # Secret env-file (sops): the GitHub App client id + private-key pointer, and the
      # Telegram credentials. This was optional (`-`) while the pass was read-only, so
      # the service could start before the App credentials existed. It is mandatory now
      # that `--apply` is on: a mutating pass with no GitHub credentials cannot push and
      # cannot escalate, so failing to start is better than running blind.
      EnvironmentFile = [ config.sops.templates."cresset-sync.env".path ];
      # See the rollout block above for what each switch does and why `--agent-apply`
      # is absent.
      #
      # `--repo-root` is the canonical bare repo itself, and `--canonical-repo` is
      # deliberately absent: it defaults to the same store, which is the point — the
      # import advances the very ref the export then plans against, so there is no
      # window in which the two can disagree.
      ExecStart = ''
        ${cresset-sync}/bin/cresset-sync \
          --repo-root ${canonicalRepo} \
          --db ${stateDb} \
          run --apply \
          --agent-command ${pkgs.claude-code}/bin/claude
      '';
      # ---- GUARDRAIL: bound the worker cgroup ----
      # A git pack/repack of a Magento-sized snapshot is the OOM risk. This cap
      # covers the worker AND its git/jj children (same cgroup); combined with
      # the bounded pack tuning below and zram swap, a spike is throttled/killed
      # inside the unit rather than OOM-killing the box (which also runs Authentik
      # + PostgreSQL).
      # The canonical repo is shared between two accounts: pushes arrive over SSH as
      # `git`, and this worker advances refs locally as `cresset-sync` (a member of the
      # `git` group). `git init --shared=group` sets core.sharedRepository so git widens
      # permissions on what it writes — but the worker's imports go through JJ, whose
      # gitoxide backend does not implement core.sharedRepository and falls back to the
      # process umask. systemd defaults that to 0022, which strips group write.
      #
      # The result was object fanout directories created 2755 instead of 2775, owned by
      # cresset-sync. Everything kept working until the next push, which failed in a way
      # that names neither permissions nor the worker:
      #
      #   refs/heads/main (reason: unable to migrate objects to permanent storage)
      #
      # 0002 makes everything this unit creates group-writable, whichever tool writes it.
      UMask = "0002";
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
    # RequiresMountsFor is a [Unit] directive. It sat in serviceConfig until systemd was
    # caught rejecting it -- "Unknown key 'RequiresMountsFor' in section [Service],
    # ignoring" -- so this guard was silently absent on every unit that declared it. The
    # /srv gating held anyway, on `requires = [ "srv.mount" ]`; this restores the second
    # layer it was always meant to have.
    unitConfig.RequiresMountsFor = "/srv";
    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = user;
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

  # ---- GUARDRAIL: mirror garbage collection ----
  #
  # The per-project bare mirrors accumulate two kinds of garbage, and git cleans neither on
  # its own in a repository nothing ever runs `gc` in:
  #
  #   1. `objects/pack/tmp_*` from fetches that die partway. `git gc` only removes these once
  #      they are a day old, so a repository that is never gc'd keeps every one for ever.
  #   2. Whole monorepo commits. The exporter brings a commit's mapped subtree into the mirror
  #      with `git fetch <monorepo> <sha>`, and fetching a commit brings its ENTIRE tree and
  #      ancestry -- every other project, `upstream/`, `experiments/`, `docs/`. They are
  #      unreachable from `refs/heads/main` so they are never pushed (verified: the GitHub
  #      repo is 641 KB and carries none of it), but they sit on disk until collected.
  #
  # On 2026-08-07 this filled /srv and wedged the worker: `infra.git` had reached 11G against
  # 641 KB of real content -- 8 GiB of tmp_* garbage plus 2 GiB of orphaned monorepo objects.
  # A single `gc --prune=now` took it to 1.7M, and the whole mirror tree from 14G to 2.2G.
  # The disk being full is also why it could not fix itself: `git gc` needs room to write a
  # new pack before it can free the old one, so the failure mode is a deadlock, not a warning.
  #
  # Daily, because the observed leak rate filled 20G in about a week. This treats the symptom;
  # the exporter should be fetching only the mapped subtree rather than whole commits, which
  # is a change to make deliberately rather than during an outage.
  systemd.services.cresset-sync-gc = {
    description = "Garbage-collect the cresset-sync downstream mirrors";
    after = [ "srv.mount" ];
    requires = [ "cresset-sync-setup.service" ];
    unitConfig.RequiresMountsFor = "/srv";
    serviceConfig = {
      Type = "oneshot";
      User = user;
      Group = user;
      # Same reason as the worker: jj/gitoxide ignores core.sharedRepository, so anything
      # writing here must leave group-writable objects behind or the next pass fails with
      # "unable to migrate objects to permanent storage".
      UMask = "0002";
      # Serialised against the worker through the worker's OWN lease lock, not through
      # systemd's `Conflicts=`.
      #
      # `Conflicts=` was the first attempt and it is the wrong tool: it STOPS the other unit
      # rather than waiting for it, so the first gc run killed a pass mid-flight
      # (`code=killed, status=15/TERM`) and left the service failed. `flock` on the same file
      # the worker locks makes them take turns instead -- gc waits for the pass to finish, and
      # a pass that fires while gc is running finds the lease held and skips that tick.
      #
      # Waiting rather than skipping, because a gc that gives up quietly is a gc that never
      # runs on a busy worker, which is exactly how the disk filled. Fifteen minutes is far
      # beyond a pass (~20s) and short of the daily cadence; failing to acquire in that window
      # means something is stuck and should be visible in `systemctl --failed`.
      ExecStart = pkgs.writeShellScript "cresset-sync-gc" ''
        set -u
        exec ${pkgs.util-linux}/bin/flock --exclusive --timeout 900 ${syncDir}/lease.lock \
          ${pkgs.writeShellScript "cresset-sync-gc-locked" ''
        set -u
        failed=0
        for repo in ${syncDir}/mirrors/*.git; do
          [ -d "$repo" ] || continue
          before=$(${pkgs.coreutils}/bin/du -sm "$repo" | ${pkgs.coreutils}/bin/cut -f1)
          if ${pkgs.git}/bin/git -C "$repo" gc --prune=now --quiet; then
            after=$(${pkgs.coreutils}/bin/du -sm "$repo" | ${pkgs.coreutils}/bin/cut -f1)
            echo "$(${pkgs.coreutils}/bin/basename "$repo"): ''${before}M -> ''${after}M"
          else
            # One broken mirror must not stop the rest: the point of this unit is that the
            # OTHER thirty keep from filling the disk.
            echo "$(${pkgs.coreutils}/bin/basename "$repo"): gc FAILED" >&2
            failed=1
          fi
        done
        ${pkgs.coreutils}/bin/df -h ${syncDir} | ${pkgs.coreutils}/bin/tail -1
        exit "$failed"
        ''}
      '';
      NoNewPrivileges = true;
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
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

  systemd.timers.cresset-sync-gc = {
    description = "Garbage-collect the cresset-sync mirrors daily";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "03:20";
      Persistent = true;
      RandomizedDelaySec = "20m";
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
