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
    ./mageos-maker.nix
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
  # IPv4: Hetzner DHCPs an address; dhcpcd default behaviour is fine.
  # IPv6: Hetzner routes a /64 to the server but does NOT run DHCPv6,
  # so the address has to be configured statically. Convention is
  # <prefix>::1, with the gateway reached via the link-local fe80::1
  # on the server-facing interface (Hetzner sends RAs advertising it,
  # but pinning it statically removes any RA-timing dependency at boot).
  # The interface name `enp1s0` is what virtio-net comes up as on
  # CAX11; if it ever changes, the deploy fails fast with "interface
  # not found" rather than silently dropping v6.
  networking = {
    hostName = "origin";
    domain = "bougie.tools";
    useDHCP = lib.mkDefault true;
    interfaces.enp1s0.ipv6.addresses = [
      { address = "2a01:4f8:c014:cfef::1"; prefixLength = 64; }
    ];
    defaultGateway6 = {
      address = "fe80::1";
      interface = "enp1s0";
    };
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
  # /srv so the publish pipeline can write the snapshot tree
  # (DISTRIBUTION.md "Hosting" three-phase rsync). No shell access
  # needed beyond what rsync uses; restrict to rsync via
  # authorized_keys command= if you want belt-and-braces.
  users.groups.deploy = {};
  users.users.deploy = {
    isNormalUser = true;
    group = "deploy";
    home = "/home/deploy";
    description = "CI publish account";
    shell = pkgs.bashInteractive;
    openssh.authorizedKeys.keys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHHigF4N4lR0UuIXB+bM7Mr52PMGurKPoe0Yjld3U/QB bougie CI publish"
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDC+gfeoHgjUZEHWKhGW4UJq8z2GnfP43LBViQBP7y03 modulargento mirror"
    ];
  };

  # ---- /srv setup + initial index ----
  # Single root-owned one-shot that:
  #   1. Chowns /srv to deploy (the volume comes up root-owned from
  #      mkfs; tmpfiles can't reliably handle this because `nofail` on
  #      the mount removes /srv from local-fs.target's dependencies, so
  #      tmpfiles can fire before the volume is mounted and chown the
  #      hidden underlying root dir instead).
  #   2. Creates the snapshot-model layout: /srv/index/ (the index
  #      vhost docroot), /srv/blobs/ (the blob vhost docroot), and
  #      /srv/releases/{github,installers} (the bougie binary mirror
  #      docroot — see nginx.nix for the vhost + cache policy and
  #      bougie's publish-mirror.yml for the writer side).
  #   3. Seeds an empty initial /srv/index/index.json so nginx 200s
  #      from day zero. The publish pipeline replaces this on its
  #      first run (DISTRIBUTION.md "Hosting" three-phase rsync).
  #   4. One-time migration: if the legacy /srv/index symlink is
  #      present (pointing into /srv/index-versions/...), replace it
  #      with a real directory so the publish pipeline can write into
  #      it. The old /srv/index-versions/ tree is left alone — GC by
  #      hand once stale roots have aged out.
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

      # Migrate legacy /srv/index symlink → real directory. The
      # publish pipeline writes into /srv/index/{versions,targets,…},
      # which a symlink to a frozen versioned tree would silently
      # forward into the wrong place.
      if [ -L /srv/index ]; then
        ${pkgs.coreutils}/bin/rm /srv/index
      fi

      install -d -o deploy -g deploy -m 0755 /srv/index /srv/blobs /srv/modulargento
      # bougie binary mirror tree. Both prefixes have to exist before
      # nginx starts so the `/github/` and `/installers/` location
      # blocks don't 404 on the first request (release.bougie.tools is
      # otherwise empty until the first publish-mirror.yml run).
      install -d -o deploy -g deploy -m 0755 \
        /srv/releases \
        /srv/releases/github \
        /srv/releases/github/bougie \
        /srv/releases/github/bougie/releases \
        /srv/releases/github/bougie/releases/download \
        /srv/releases/installers \
        /srv/releases/installers/bougie \
        /srv/releases/installers/bougie/latest
      if [ ! -e /srv/index/index.json ]; then
        cat > /srv/index/index.json <<'JSON'
      {
        "schema": 1,
        "version": "00000000T000000Z",
        "generated": "2024-01-01T00:00:00Z",
        "source": { "git_commit": "unknown", "git_ref": "unknown" },
        "targets": {}
      }
      JSON
        chown deploy:deploy /srv/index/index.json
      fi
    '';
  };

  # nginx serves /srv/index — must wait for it to exist.
  systemd.services.nginx = {
    after = [ "bootstrap-index.service" ];
    requires = [ "bootstrap-index.service" ];
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
