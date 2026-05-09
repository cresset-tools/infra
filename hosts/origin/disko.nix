# Declarative disk layout, applied by nixos-anywhere on first install.
#
# Hetzner CAX11 (ARM, smallest tier) ships one ~40 GB QEMU virtio disk
# as the boot device, plus the Hetzner Cloud Volume we attach for /srv.
# Both go through their /dev/disk/by-id/ paths because the kernel's
# /dev/sdX enumeration is unstable across reboots and across volume
# attach/detach (with a volume attached, /dev/sda may be the volume and
# the boot disk shifts to /dev/sdb).
#
# CAX11 boots UEFI-only (no legacy BIOS). One ESP + one ext4 root, no
# BIOS-boot partition.
#
# Identifiers: both serials are per-instance (assigned at server/volume
# creation, baked into the QEMU/SCSI disk id). They're not secrets —
# Hetzner exposes them in the console — but they ARE host-specific, so
# rebuilding either the server or the volume requires updating the
# corresponding value here.
{ ... }:
let
  # TODO: replace with your Hetzner Cloud Volume's numeric ID. Visible
  # in the Hetzner Cloud console URL when viewing the volume, or via
  # `hcloud volume list`. For example: 102934857.
  volumeId = "105660807";

  # TODO: replace with the boot disk's QEMU serial. Find it from rescue
  # mode with `lsblk -o NAME,SIZE,MODEL,SERIAL` — the row whose MODEL is
  # "QEMU HARDDISK" and SIZE matches the server's primary disk. Or:
  # `ls /dev/disk/by-id/ | grep QEMU_HARDDISK`.
  bootDiskSerial = "117928016";
in
{
  disko.devices = {
    disk.main = {
      type = "disk";
      device = "/dev/disk/by-id/scsi-0QEMU_QEMU_HARDDISK_${bootDiskSerial}";
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
              # -F forces mkfs.ext4 to overwrite an existing signature.
              # Defends against re-deploys where prior partition data
              # leaves stale ext4 metadata at the same offset.
              extraArgs = [ "-F" ];
            };
          };
        };
      };
    };

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
            extraArgs = [ "-F" ];
          };
        };
      };
    };
  };
}
