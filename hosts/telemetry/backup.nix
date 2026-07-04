# Offsite backups: borg → Hetzner Storage Box.
#
# Box: `telemetry-backups` (BX11, 1 TiB, fsn1), user u627005. Created
# via `hcloud storage-box create`; `reachable_externally` is false, so
# only hosts inside Hetzner's network — like this one — can reach it.
# On the new cloud Storage Boxes the SSH key is installed by uploading
# `.ssh/authorized_keys` over password SFTP once (the legacy
# `install-ssh-key` command doesn't exist there, and password auth is
# refused from outside Hetzner — a decoy prompt, by design).
#
# Secrets live on this host only, out of git:
#   /root/borg/ssh          dedicated ed25519 key; pub half is the
#                           box's authorized_keys
#   /root/borg/passphrase   borg repokey passphrase. MUST also live
#                           off-box (password manager): after a total
#                           host loss this passphrase is the only way
#                           into the backups.
#
# The job never touches the live database: a consistent sqlite
# `.backup` snapshot is staged first (WAL-safe while the collector
# runs), so a mid-write borg pass can't capture a torn file.
{ config, pkgs, ... }:
{
  services.borgbackup.jobs.collector = {
    preHook = ''
      mkdir -p /var/lib/borg-stage
      ${pkgs.sqlite}/bin/sqlite3 /var/lib/bougie-collector/collector.db \
        ".backup /var/lib/borg-stage/collector.db"
    '';
    paths = [ "/var/lib/borg-stage" ];
    repo = "ssh://u627005@u627005.your-storagebox.de:23/./bougie-collector";
    encryption = {
      mode = "repokey-blake2";
      passCommand = "cat /root/borg/passphrase";
    };
    environment = {
      BORG_RSH = "ssh -i /root/borg/ssh -o StrictHostKeyChecking=accept-new";
    };
    compression = "auto,zstd";
    startAt = "*-*-* 05:15:00 UTC";
    prune.keep = {
      daily = 14;
      weekly = 8;
      monthly = 12;
    };
  };
}
