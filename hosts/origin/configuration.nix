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

  # Root SSH keys for nixos-rebuild deploys from your laptop. Replace the
  # placeholder with your actual ed25519 public key before first install.
  # See README.md "Bootstrap" for the full sequence.
  users.users.root.openssh.authorizedKeys.keys = [
    # TODO: replace with `cat ~/.ssh/id_ed25519.pub` (or whichever key you
    # use for ops). Keeping the placeholder commented prevents an empty
    # authorized_keys from accidentally locking you out.
    # "ssh-ed25519 AAAA... jelle@laptop"
  ];

  # ---- The deploy user ----
  # CI's rsync-publish-tree app SSHes here as `deploy`. The user owns
  # /srv so it can rsync into /srv/index-versions/<VERSION>/ and create
  # the /srv/index symlink atomically. No shell access needed beyond
  # what rsync uses; restrict to rsync via authorized_keys command= if
  # you want belt-and-braces.
  users.users.deploy = {
    isNormalUser = true;
    home = "/home/deploy";
    description = "CI publish account";
    shell = pkgs.bashInteractive;
    openssh.authorizedKeys.keys = [
      # TODO: replace with the public half of secrets.PUBLISH_SSH_KEY in
      # cresset-tools/php-build-standalone GitHub Actions secrets.
      # "ssh-ed25519 AAAA... cresset CI publish"
    ];
  };

  # /srv is the document root for nginx. The deploy user owns it so
  # rsync + atomic symlink flip works without sudo.
  systemd.tmpfiles.rules = [
    "d /srv 0755 deploy deploy -"
    "d /srv/index-versions 0755 deploy deploy -"
    "d /srv/blobs 0755 deploy deploy -"
  ];

  # ---- Bootstrap an empty initial index so nginx serves 200 from day
  # zero, before the first CI publish lands. The first real publish
  # replaces this version and flips the symlink. ----
  systemd.services.bootstrap-index = {
    description = "Seed an empty initial index version + symlink";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      User = "deploy";
      Group = "deploy";
    };
    script = ''
      set -euo pipefail
      target=/srv/index-versions/initial
      if [ ! -e "$target/index.json" ]; then
        mkdir -p "$target"
        cat > "$target/index.json" <<'JSON'
      {
        "schema": 1,
        "generated": "2024-01-01T00:00:00Z",
        "targets": {}
      }
      JSON
      fi
      if [ ! -L /srv/index ]; then
        ln -s "$target" /srv/index.new
        ${pkgs.coreutils}/bin/mv -T /srv/index.new /srv/index
      fi
    '';
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
