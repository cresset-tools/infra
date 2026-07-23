# Declarative disk layout for the relay box, applied by nixos-anywhere on
# first install.
#
# Hetzner CX23 (x86, cheapest current tier) ships one ~40 GB virtio disk and
# boots legacy BIOS. GPT + a 1 MiB BIOS-boot partition for GRUB's stage rather
# than an ESP. No Cloud Volume is attached, so /dev/sda is unambiguous and we
# skip the per-instance disk-serial dance. (Mirrors hosts/telemetry.)
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
              # -F overwrites any stale ext4 signature on re-deploys.
              extraArgs = [ "-F" ];
            };
          };
        };
      };
    };
  };
}
