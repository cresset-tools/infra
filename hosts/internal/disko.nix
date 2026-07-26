{ ... }:
let
  # TODO: REPLACE_WITH_HETZNER_VOLUME_ID — the Hetzner Cloud Volume's numeric
  # ID for the /srv volume attached to `internal`. Visible in the Hetzner Cloud
  # console URL when viewing the volume, or via `hcloud volume list`. Mirrors the
  # `volumeId` placeholder in hosts/origin/disko.nix. For example: 102934857.
  volumeId = "REPLACE_WITH_HETZNER_VOLUME_ID";
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
