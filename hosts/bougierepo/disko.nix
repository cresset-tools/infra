# Declarative disk layout for the bougierepo box, applied by nixos-anywhere on
# first install.
#
# Hetzner cpx22 (x86, AMD) ships one virtio disk and boots **UEFI** — verified
# via /sys/firmware/efi on the box, unlike the Intel cx/ccx fleet (telemetry,
# bougie-relay) which is legacy BIOS. So this host uses a FAT32 ESP at /boot for
# systemd-boot, NOT an EF02 BIOS-boot stage + GRUB (which left the UEFI firmware
# with nothing to boot). No Cloud Volume is attached, so /dev/sda is unambiguous.
{ ... }:
{
  disko.devices = {
    disk.main = {
      type = "disk";
      device = "/dev/sda";
      content = {
        type = "gpt";
        partitions = {
          ESP = {
            size = "512M";
            type = "EF00"; # EFI System Partition
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
              # -F overwrites any stale ext4 signature on re-deploys.
              extraArgs = [ "-F" ];
            };
          };
        };
      };
    };
  };
}
