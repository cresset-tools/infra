# vmbox-1: two 477G NVMe drives. System on the first (BIOS/GRUB, ext4 per
# repo convention); the second is the state disk for the testing pipeline
# (worktree, sandboxes, bougie home, reports) mounted at /srv.
{
  disko.devices.disk = {
    main = {
      type = "disk";
      device = "/dev/nvme0n1";
      content = {
        type = "gpt";
        partitions = {
          boot = {
            size = "1M";
            type = "EF02";
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
    state = {
      type = "disk";
      device = "/dev/nvme1n1";
      content = {
        type = "gpt";
        partitions.srv = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/srv";
            mountOptions = [ "defaults" "nofail" "x-systemd.device-timeout=10s" ];
            extraArgs = [ "-F" ];
          };
        };
      };
    };
  };
}
