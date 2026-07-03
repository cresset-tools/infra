# Declarative disk layout, applied by nixos-anywhere on first install.
#
# Hetzner CX23 (x86, cheapest current tier) ships one ~40 GB virtio
# disk and boots legacy BIOS. GPT + a 1 MiB BIOS-boot partition for
# GRUB's stage rather than an ESP. Unlike origin, no Cloud Volume is
# attached, so /dev/sda is unambiguous and we can skip the
# per-instance disk-serial dance.
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
              # -F overwrites stale signatures on re-deploys.
              extraArgs = [ "-F" ];
            };
          };
        };
      };
    };
  };
}
