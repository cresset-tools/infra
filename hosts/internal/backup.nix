# Offsite backups for `internal`: borg → the existing Hetzner Storage Box.
#
# Until 2026-08-07 this host had none. The only thing resembling a backup was
# `services.postgresqlBackup`, which writes Authentik dumps to /var/backup/postgresql on the
# SAME disk as everything else — a dump, not a backup. It dies with the volume, and the volume
# had just been at 100% with the canonical monorepo on it.
#
# ## What is actually irreplaceable
#
# 42 of ~88,000 tracked files sit outside every mapped root, so most CONTENT survives on
# GitHub. That undersells the risk, because three things exist only here:
#
#   - The unified history. GitHub holds 31 per-project histories of rewritten commits;
#     /srv/git/cresset.git holds the real jj history — change ids, the operation log, the
#     interleaved commits. Rebuilding from the mirrors would produce a different repository,
#     not a restore.
#   - docs/, deliberately unmapped, which `.sync/projects.toml` describes as holding
#     unannounced products, pricing and commercial strategy.
#   - /srv/sync/state.db: checkpoints and the resolution history. Losing it means
#     re-bootstrapping all 32 projects and losing every recorded conflict resolution.
#
# Plus /etc/ssh/ssh_host_ed25519_key, which `sops.age.sshKeyPaths` makes the master key for
# every secret on this host.
#
# ## What is deliberately NOT backed up
#
# /srv/sync/mirrors — 2 GB of bare clones of public GitHub repositories, reconstructible by
# re-fetching. Backing them up would triple the backup for nothing.
#
# ## Credentials
#
# Unlike hosts/telemetry, both live in sops rather than as hand-placed files under /root:
#
#   borg/ssh_key      dedicated ed25519 key; its public half is in the Storage Box's
#                     .ssh/authorized_keys alongside telemetry's
#   borg/passphrase   borg repokey passphrase
#
# This is the difference that matters after a TOTAL host loss. Telemetry's passphrase lives
# only on telemetry and in whatever password manager someone remembered to use; if both are
# gone the backups are unopenable, and a backup you cannot open is not a backup. These are
# encrypted to the admin age keys as well as the host key, so the recovery path is: admin key
# (on a laptop) → decrypt from git → open the repo. It does not depend on the host that died.
# ## Restoring
#
# Verified end to end on 2026-08-07, not merely designed. Restored `main` matched live at
# e746dd6b062c across 2,997 commits, docs/planning came back intact, and the checkpoint
# database passed integrity_check with all 32 projects.
#
#   export BORG_PASSCOMMAND="cat /run/secrets/borg/passphrase"   # or sops -d from git
#   export BORG_RSH="ssh -i /run/secrets/borg/ssh_key"
#   R=ssh://u627005@u627005.your-storagebox.de:23/./cresset-internal
#   borg list "$R"                                   # pick an archive
#   borg extract "$R::<archive>" var/lib/borg-stage  # extracts under $PWD
#   git clone --bare var/lib/borg-stage/cresset.bundle cresset.git
#
# From a DEAD host, the credentials come from git instead: the secrets are encrypted to the
# admin age keys as well as this host's, so `sops -d --extract '["borg"]["passphrase"]'
# secrets/internal.yaml` on a laptop opens the repo. That is the whole point of keeping them
# there rather than in /root.
#
# The Storage Box is `reachable_externally = false`, so a restore has to run from inside
# Hetzner's network — from a replacement host, or from telemetry, which already has access.
{ config, pkgs, ... }:
let
  stage = "/var/lib/borg-stage";
  canonical = "/srv/git/cresset.git";
  syncDb = "/srv/sync/state.db";
  # Review discussion. Not reconstructible from anything: unlike the checkpoint database,
  # which could be rebuilt by re-bootstrapping every project, a comment exists only here.
  reviewDb = "/var/lib/cresset-view/review.db";
  # The worker's own single-instance lock. Taking it means no pass can be advancing `main` on
  # the canonical repo, or writing the checkpoint database, while they are being read.
  lease = "/srv/sync/lease.lock";
in
{
  sops.secrets."borg/ssh_key" = {
    mode = "0400";
    owner = "root";
  };
  sops.secrets."borg/passphrase" = {
    mode = "0400";
    owner = "root";
  };

  # The borg unit runs under ProtectSystem=strict; the stage dir must exist up front.
  systemd.tmpfiles.rules = [ "d ${stage} 0700 root root -" ];

  services.borgbackup.jobs.internal = {
    # Stage consistent copies rather than reading live state.
    #
    # Both reads are taken under the worker's lease, so nothing can be half-written: a bundle
    # captured while an import advances `main` would be internally consistent as a git object
    # graph but might not contain the commit the checkpoint claims, which is the one
    # inconsistency that makes a restore silently wrong rather than obviously broken.
    preHook = ''
      set -eu
      ${pkgs.util-linux}/bin/flock --exclusive --timeout 900 ${lease} \
        ${pkgs.writeShellScript "cresset-backup-stage" ''
          set -eu
          # `safe.directory` because this runs as root against a repository owned by `git`,
          # and git's dubious-ownership guard refuses it. Scoped to the one invocation rather
          # than set globally, so it grants exactly this. Worth knowing that the refusal
          # surfaces as "fatal: Need a repository to create a bundle", which reads like the
          # path is wrong rather than like a permission decision.
          git="${pkgs.git}/bin/git -c safe.directory=${canonical} -C ${canonical}"
          # A bundle, not a copy of the directory: one self-contained file that `git clone`
          # restores directly, and that does not change shape when gc repacks.
          $git bundle create ${stage}/cresset.bundle --all
          # Verify before it becomes the only copy. A bundle that cannot be read is worth
          # discovering now, not during a restore.
          $git bundle verify ${stage}/cresset.bundle >/dev/null
          # Copy the database and its WAL sidecars, then consolidate in the STAGE directory.
          #
          # `.backup` against the live file cannot work here whichever way it is opened: WAL
          # mode needs to map a `-shm` beside the source, and `ProtectSystem=strict` leaves
          # /srv read-only for this unit. `-readonly` does not avoid that -- it failed with
          # "unable to open database file".
          #
          # Copying under the lease is consistent because there is no writer, and it touches
          # nothing in /srv. In practice SQLite has already checkpointed by the time the last
          # connection closed, so the sidecars are usually absent or empty -- but "usually" is
          # not a property to build a backup on, so take them when they are there.
          ${pkgs.coreutils}/bin/cp -p ${syncDb} ${stage}/state.db
          for sidecar in -wal -shm; do
            if [ -e "${syncDb}$sidecar" ]; then
              ${pkgs.coreutils}/bin/cp -p "${syncDb}$sidecar" "${stage}/state.db$sidecar"
            fi
          done
          # Fold any WAL into the copy and prove the result is sound, both inside the writable
          # stage. A restore then needs one file, and a database that cannot be opened is
          # discovered now rather than when it is the only one left.
          ${pkgs.sqlite}/bin/sqlite3 ${stage}/state.db "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
          # The review store, the same way. Absent on an instance where review was never
          # enabled, which is not an error.
          if [ -e ${reviewDb} ]; then
            ${pkgs.coreutils}/bin/cp -p ${reviewDb} ${stage}/review.db
            for sidecar in -wal -shm; do
              if [ -e "${reviewDb}$sidecar" ]; then
                ${pkgs.coreutils}/bin/cp -p "${reviewDb}$sidecar" "${stage}/review.db$sidecar"
              fi
            done
            ${pkgs.sqlite}/bin/sqlite3 ${stage}/review.db "PRAGMA wal_checkpoint(TRUNCATE);" >/dev/null
            if [ "$(${pkgs.sqlite}/bin/sqlite3 ${stage}/review.db 'PRAGMA integrity_check;')" != "ok" ]; then
              echo "staged review database failed integrity_check; refusing to back it up" >&2
              exit 1
            fi
          fi
          if [ "$(${pkgs.sqlite}/bin/sqlite3 ${stage}/state.db 'PRAGMA integrity_check;')" != "ok" ]; then
            echo "staged checkpoint database failed integrity_check; refusing to back it up" >&2
            exit 1
          fi
        ''}
    '';
    paths = [
      stage
      # Authentik's dumps. `services.postgresqlBackup` writes them here daily at 04:30; this
      # job runs after, so it always carries a fresh one.
      "/var/backup/postgresql"
      # Host keys — including ssh_host_ed25519_key, the sops master key for this host.
      "/etc/ssh"
    ];
    readWritePaths = [ stage ];
    repo = "ssh://u627005@u627005.your-storagebox.de:23/./cresset-internal";
    encryption = {
      mode = "repokey-blake2";
      passCommand = "cat ${config.sops.secrets."borg/passphrase".path}";
    };
    environment = {
      BORG_RSH = "ssh -i ${config.sops.secrets."borg/ssh_key".path} -o StrictHostKeyChecking=accept-new";
    };
    compression = "auto,zstd";
    # After postgresqlBackup (04:30) and after the mirror gc (03:20), so the dump is current
    # and the repo is not being repacked underneath the bundle.
    startAt = "*-*-* 05:45:00 UTC";
    prune.keep = {
      daily = 14;
      weekly = 8;
      monthly = 12;
    };
  };
}
