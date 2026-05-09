# Hetzner CAX11 origin server (ARM/aarch64, smallest tier). Serves the
# bougie.tools distribution index from /srv (a Hetzner Cloud Volume) via
# nginx. CI publishes to it over SSH as the `deploy` user; every
# artifact below the root is content-addressed and immutable, so nginx
# is just a glorified static file server with smart cache headers.
#
# Storage: 40 GB built-in SSD for the OS, + a separate Hetzner Cloud
# Volume mounted at /srv for distribution data. Resize the volume in
# Hetzner UI without touching the boot disk; rsync resumes against the
# enlarged surface immediately.
{ config, pkgs, lib, modulesPath, ... }:
{
  imports = [
    ./nginx.nix
  ];

  # ---- Bootloader ----
  # CAX11 is UEFI-only (no legacy BIOS). systemd-boot is the simplest
  # NixOS bootloader for UEFI and works on both ARM and x86.
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

  # Kernel boot output goes to both the VGA console (visible in
  # Hetzner's noVNC view) and ttyAMA0 (the ARM serial port that
  # Hetzner's "Console" tab pipes through). Without this, ARM kernels
  # often go silent after systemd-boot hands off, and a black VNC is
  # indistinguishable from a kernel panic.
  boot.kernelParams = [
    "console=tty1"
    "console=ttyAMA0,115200"
  ];

  # Hetzner CAX exposes the boot disk over virtio-scsi (see the
  # /dev/disk/by-id/scsi-0QEMU_QEMU_HARDDISK_… path) and the network
  # over virtio-net. Without these in the initrd, stage-1 can't find
  # /dev/disk/by-partlabel/disk-main-root and the boot stalls silently.
  # The ahci/xhci/sd_mod/sr_mod entries are the standard cloud-VM
  # baseline — cheap belt-and-braces for any QEMU-based host.
  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_scsi"
    "virtio_blk"
    "virtio_net"
    "ahci"
    "xhci_pci"
    "sd_mod"
    "sr_mod"
    "usbhid"
  ];

  # ---- Networking ----
  networking = {
    hostName = "origin";
    domain = "bougie.tools";
    # Hetzner DHCPs an IPv4 + assigns a /64 IPv6 prefix; defaults are fine.
    useDHCP = lib.mkDefault true;
    firewall = {
      enable = true;
      allowedTCPPorts = [
        22   # SSH
        80   # ACME HTTP-01 + HTTPS redirect
        443  # HTTPS
      ];
    };
  };

  # ---- Time + locale ----
  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  # ---- SSH ----
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";  # key-only for nixos-rebuild
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
  };

  # Fully declarative user management: this config is the only source of
  # truth for users/groups; anything created by `useradd` or `passwd`
  # outside it gets purged on next switch. SSH-key auth only — no
  # passwords are set, which is consistent with PasswordAuthentication=no
  # above.
  users.mutableUsers = false;

  # Root SSH keys for nixos-rebuild deploys from your laptop. Replace the
  # placeholder with your actual ed25519 public key before first install.
  # See README.md "Bootstrap" for the full sequence.
  users.users.root.openssh.authorizedKeys.keys = [
    # TODO: replace with `cat ~/.ssh/id_ed25519.pub` (or whichever key you
    # use for ops). Keeping the placeholder commented prevents an empty
    # authorized_keys from accidentally locking you out.
    # "ssh-ed25519 AAAA... jelle@laptop"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
  ];

  # ---- The deploy user ----
  # CI's rsync-publish-tree app SSHes here as `deploy`. The user owns
  # /srv so it can rsync into /srv/index-versions/<VERSION>/ and create
  # the /srv/index symlink atomically. No shell access needed beyond
  # what rsync uses; restrict to rsync via authorized_keys command= if
  # you want belt-and-braces.
  users.groups.deploy = {};
  users.users.deploy = {
    isNormalUser = true;
    group = "deploy";
    home = "/home/deploy";
    description = "CI publish account";
    shell = pkgs.bashInteractive;
    openssh.authorizedKeys.keys = [
      # TODO: replace with the public half of secrets.PUBLISH_SSH_KEY in
      # cresset-tools/php-build-standalone GitHub Actions secrets.
      # "ssh-ed25519 AAAA... cresset CI publish"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIE3JLt9h4tZvZ1Hl3m7q8lKJKpgnnLuCoBunlG9AxH24 bougie CI publish"
    ];
  };

  # ---- /srv setup + initial index ----
  # Single root-owned one-shot that:
  #   1. Chowns /srv to deploy (the volume comes up root-owned from
  #      mkfs; tmpfiles can't reliably handle this because `nofail` on
  #      the mount removes /srv from local-fs.target's dependencies, so
  #      tmpfiles can fire before the volume is mounted and chown the
  #      hidden underlying root dir instead).
  #   2. Creates index-versions/ and blobs/ on the mounted volume.
  #   3. Seeds an empty initial index so nginx 200s from day zero.
  #   4. Creates the /srv/index symlink the publish pipeline flips.
  # nginx is ordered after this so the document root exists before the
  # daemon starts.
  systemd.services.bootstrap-index = {
    description = "Initialise /srv layout + seed empty initial index";
    wantedBy = [ "multi-user.target" ];
    # Order after the volume mount specifically — local-fs.target is
    # not enough because nofail excludes /srv from its requires set.
    after = [ "srv.mount" ];
    requires = [ "srv.mount" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # Runs as root because chowning /srv requires it; after the
      # script the entire /srv subtree is deploy-owned.
    };
    script = ''
      set -euo pipefail
      chown deploy:deploy /srv
      chmod 0755 /srv
      install -d -o deploy -g deploy -m 0755 /srv/index-versions /srv/blobs
      target=/srv/index-versions/initial
      if [ ! -e "$target/index.json" ]; then
        install -d -o deploy -g deploy -m 0755 "$target"
        cat > "$target/index.json" <<'JSON'
      {
        "schema": 1,
        "generated": "2024-01-01T00:00:00Z",
        "targets": {}
      }
      JSON
        chown deploy:deploy "$target/index.json"
      fi
      if [ ! -L /srv/index ]; then
        ln -s "$target" /srv/index.new
        ${pkgs.coreutils}/bin/mv -T /srv/index.new /srv/index
      fi
    '';
  };

  # nginx serves /srv/index — must wait for it to exist.
  systemd.services.nginx = {
    after = [ "bootstrap-index.service" ];
    requires = [ "bootstrap-index.service" ];
  };

  # ---- Hardening ----
  services.fail2ban = {
    enable = true;
    maxretry = 5;
    bantime = "1h";
    bantime-increment.enable = true;
    ignoreIP = [ "127.0.0.1/8" "::1" ];
  };

  # Unattended security updates. Sunday at 03:30 UTC the box re-fetches
  # this flake from GitHub at HEAD-of-main, rebuilds, switches, and
  # reboots if the kernel changed. The server has no push credentials,
  # so it never mutates flake.lock — lock bumps come from the
  # update-flake-lock workflow (.github/workflows/update-flake-lock.yml)
  # which opens a PR every Saturday morning. Merge before Sunday 03:30
  # UTC and the bump rolls onto the box on the next timer fire.
  system.autoUpgrade = {
    enable = true;
    flake = "github:cresset-tools/infra#origin";
    dates = "Sun 03:30";
    randomizedDelaySec = "30min";
    allowReboot = true;
    rebootWindow = { lower = "03:00"; upper = "05:00"; };
  };

  # Journal cap so logs don't fill the disk on a low-touch box.
  services.journald.extraConfig = ''
    SystemMaxUse=1G
    MaxRetentionSec=2week
  '';

  # ---- Tools handy on the box ----
  environment.systemPackages = with pkgs; [
    rsync git curl jq htop tmux
  ];

  # NixOS state version — pin to the version this config was first
  # deployed against. Don't change this on upgrades.
  system.stateVersion = "25.11";
}
