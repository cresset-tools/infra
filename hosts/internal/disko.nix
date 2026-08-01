{ ... }:
let
  # The Hetzner Cloud Volume backing /srv on `internal` (`hcloud volume describe
  # internal-srv`). 20 GB: the module declares a 4 GB swapfile on this volume, and the
  # canonical repo plus 31 downstream mirrors sit alongside it, so a 10 GB volume would be
  # roughly half consumed at rest — with a git repack spike, the very failure the memory
  # guardrails exist for, writing into the same space.
  volumeId = "106515963";

  # The local system disk, addressed by its stable /dev/disk/by-id path for the SAME
  # reason the Cloud Volume below is: /dev/sdX enumeration is not stable. That is not
  # theoretical here — on the rebuilt host the volume came up as /dev/sda and the local
  # disk as /dev/sdb, the reverse of the first install.
  #
  # The running system never cared (fstab mounts everything by partlabel), but disko
  # DOES: it formats whatever `device` names. With a bare /dev/sda this module was one
  # coin flip away from installing the OS onto the 20 GB volume and putting /srv on the
  # system disk — and re-provisioning an existing host that way would destroy the very
  # volume all durable state lives on.
  #
  # Like volumeId, this is per-server (the serial is the QEMU disk's) and must be
  # refreshed when the machine is rebuilt: `ls -l /dev/disk/by-id | grep QEMU`.
  mainDiskId = "scsi-0QEMU_QEMU_HARDDISK_124275528";
in
{
  disko.devices.disk = {
    main = {
      type = "disk";
      device = "/dev/disk/by-id/${mainDiskId}";
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
