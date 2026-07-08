# Declarative disk layout, applied by nixos-anywhere on first install.
#
# Hetzner CX33 (x86, 4 vCPU / 8 GB / 80 GB disk for the Magento + OpenSearch +
# DB footprint) boots legacy BIOS. GPT + a 1 MiB BIOS-boot partition for GRUB
# rather than an ESP. One virtio disk, no Cloud Volume, so /dev/sda is
# unambiguous (same layout as telemetry).
{ ... }:
{
  disko.devices = {
    disk.main = {
      type = "disk";
      device = "/dev/sda";
      content = {
        type = "gpt";
        partitions = {
          boot = {
            size = "1M";
            type = "EF02"; # GRUB BIOS-on-GPT stage area
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
  };
}
