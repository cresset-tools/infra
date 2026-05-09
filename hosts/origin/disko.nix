# Declarative disk layout, applied by nixos-anywhere on first install.
#
# Hetzner CAX11 (ARM, smallest tier) ships one 40 GB SSD as /dev/sda. We
# also attach a Hetzner Cloud Volume that holds /srv — this lets us
# resize the distribution storage online (Hetzner UI → Volumes → Resize)
# without touching the boot disk, and the volume can be detached and
# reattached to a larger server later.
#
# CAX11 boots UEFI-only (no legacy BIOS). One ESP + one ext4 root, no
# BIOS-boot partition.
#
# The volume's stable identifier is /dev/disk/by-id/scsi-0HC_Volume_<ID>.
# Edit `volumeId` below to your numeric volume ID before deploying. The
# ID is not a secret — it's a database row id visible in the Hetzner
# console URL — so it's fine to commit.
{ ... }:
let
  # TODO: replace with your Hetzner Cloud Volume's numeric ID. Visible
  # in the Hetzner Cloud console URL when viewing the volume, or via
  # `hcloud volume list`. For example: 102934857.
  volumeId = "REPLACE_ME_WITH_NUMERIC_VOLUME_ID";
in
{
  disko.devices = {
    disk.boot = {
      type = "disk";
      device = "/dev/sda";
      content = {
        type = "gpt";
        partitions = {
          esp = {
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
            };
          };
        };
      };
    };

    # Hetzner Cloud Volume → /srv. Stable by-id path so the device
    # doesn't shift around if attachment order changes.
    disk.srv = {
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
            # nofail: if the volume is ever detached, the box still
            # boots into a degraded mode (nginx will 404 every request,
            # but ssh + autoUpgrade keep working so we can recover).
            mountOptions = [ "defaults" "nofail" "x-systemd.device-timeout=10s" ];
          };
        };
      };
    };
  };
}
