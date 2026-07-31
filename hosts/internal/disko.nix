{ ... }:
let
  # The Hetzner Cloud Volume backing /srv on `internal` (`hcloud volume describe
  # internal-srv`). 20 GB: the module declares a 4 GB swapfile on this volume, and the
  # canonical repo plus 31 downstream mirrors sit alongside it, so a 10 GB volume would be
  # roughly half consumed at rest — with a git repack spike, the very failure the memory
  # guardrails exist for, writing into the same space.
  #
  # STALE: this id belonged to the first `internal`, which was torn down along with its
  # volume. Provisioning against it would fail at disko on a device that does not exist.
  # Replace it with the new volume's id (`hcloud volume create`) before the next deploy.
  volumeId = "106511468";
in
{
  disko.devices.disk = {
    main = {
      type = "disk";
      device = "/dev/sda";
      content = {
        type = "gpt";
        partitions = {
          ESP = {
            size = "512M";
            type = "EF00";
            content = {
              type = "filesystem";
              format = "vfat";
              mountpoint = "/boot";
              mountOptions = [ "umask=0077" ];
            };
          };
          root = {
            size = "100%";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
              extraArgs = [ "-F" ];
            };
          };
        };
      };
    };

    # Hetzner Cloud Volume attached for /srv (worker + canonical-repo data).
    # Mirrors hosts/origin/disko.nix's volume block: addressed by its stable
    # /dev/disk/by-id path because /dev/sdX enumeration is unstable across
    # reboots and volume attach/detach.
    srv = {
      type = "disk";
      device = "/dev/disk/by-id/scsi-0HC_Volume_${volumeId}";
      content = {
        type = "gpt";
        partitions.srv = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/srv";
            # nofail: if the volume is ever detached, the box still boots into a
            # degraded mode so ssh + autoUpgrade keep working for recovery.
            mountOptions = [ "defaults" "nofail" "x-systemd.device-timeout=10s" ];
            extraArgs = [ "-F" ];
          };
        };
      };
    };
  };
}
