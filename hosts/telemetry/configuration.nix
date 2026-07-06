# Hetzner CX23 telemetry box (x86_64, cheapest current tier,
# ~€6.6/month). Runs bougie-collector — the first-party ingest for
# bougie's opt-in telemetry and user-initiated diagnostic reports —
# behind nginx. The contract for what this box may receive and store
# is TELEMETRY.md in cresset-tools/bougie.
#
# Deliberately its own host rather than a vhost on origin: an
# unauthenticated public ingest endpoint doesn't get to share blast
# radius with the release mirror.
#
# Privacy invariants baked into this host (TELEMETRY.md promises):
#   - client IPs are never written to disk: vhost access_log is off
#     and the vhost error_log goes to /dev/null (an upstream hiccup
#     would otherwise log "client: <ip>" lines);
#   - telemetry.bougie.tools must stay DNS-only (grey cloud) in
#     Cloudflare — proxying would terminate TLS at a third party and
#     break the first-party pledge;
#   - the collector process keeps IPs in memory for rate limiting
#     only, validates every event against the TELEMETRY.md allowlist,
#     and drops everything else.
{ config, pkgs, lib, ... }:
let
  bougie-collector = pkgs.rustPlatform.buildRustPackage {
    pname = "bougie-collector";
    version = "0.1.0";
    src = ./bougie-collector;
    cargoLock.lockFile = ./bougie-collector/Cargo.lock;
  };
in
{
  imports = [
    ./backup.nix
  ];

  # ---- Bootloader ----
  # CX-line boots legacy BIOS; GRUB with the BIOS-boot partition from
  # disko.nix. (origin's systemd-boot is UEFI-only — don't copy it.)
  # No explicit `device` here: disko derives grub's install device
  # from the EF02 partition, and setting it in both places trips the
  # mirroredBoots duplicate-device assertion.
  boot.loader.grub.enable = true;

  boot.kernelParams = [
    "console=tty1"
    "console=ttyS0,115200"
  ];

  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_scsi"
    "virtio_blk"
    "virtio_net"
    "ahci"
    "xhci_pci"
    "sd_mod"
    "sr_mod"
  ];

  # ---- Networking ----
  # Same Hetzner conventions as origin: DHCP v4; static v6 out of the
  # routed /64 with the link-local fe80::1 gateway.
  networking = {
    hostName = "telemetry";
    domain = "bougie.tools";
    useDHCP = lib.mkDefault true;
    interfaces.enp1s0.ipv6.addresses = [
      { address = "2a01:4f8:c012:653d::1"; prefixLength = 64; }
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
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
  };

  users.mutableUsers = false;
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
  ];

  # ---- The collector ----
  # DynamicUser: no fixed uid on disk, StateDirectory owns the DB dir.
  # The service is the only thing on this box that opens the database.
  systemd.services.bougie-collector = {
    description = "bougie telemetry + diagnose collector";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ];
    serviceConfig = {
      ExecStart = "${bougie-collector}/bin/bougie-collector";
      DynamicUser = true;
      StateDirectory = "bougie-collector";
      Environment = [
        "BOUGIE_COLLECTOR_DB=/var/lib/bougie-collector/collector.db"
        "BOUGIE_COLLECTOR_LISTEN=127.0.0.1:8787"
      ];
      Restart = "always";
      RestartSec = 2;
      # Hardening — same philosophy as bougie's sandbox-by-default.
      NoNewPrivileges = true;
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
      PrivateDevices = true;
      ProtectKernelTunables = true;
      ProtectKernelModules = true;
      ProtectControlGroups = true;
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
      RestrictNamespaces = true;
      LockPersonality = true;
      MemoryDenyWriteExecute = true;
      SystemCallArchitectures = "native";
      SystemCallFilter = [ "@system-service" "~@privileged" ];
      CapabilityBoundingSet = "";
    };
  };

  # ---- nginx: TLS termination only ----
  security.acme = {
    acceptTerms = true;
    defaults.email = "jelle@pingiun.com";
  };

  services.nginx = {
    enable = true;
    recommendedOptimisation = true;
    recommendedTlsSettings = true;

    virtualHosts."telemetry.bougie.tools" = {
      enableACME = true;
      forceSSL = true;

      extraConfig = ''
        # TELEMETRY.md: "IP addresses are used in memory for rate
        # limiting only and are never written to storage." That
        # includes nginx logs — keep both off for this vhost.
        access_log off;
        error_log /dev/null crit;

        # Telemetry batches cap at 256 KiB; schema-2 diagnose reports
        # (service-log tails, DIAGNOSE_PLAN.md in cresset-tools/bougie)
        # go up to 1 MiB. The collector enforces the per-route split;
        # nginx just needs to let the bigger one through.
        client_max_body_size 1m;
      '';

      locations."/" = {
        proxyPass = "http://127.0.0.1:8787";
        extraConfig = ''
          proxy_set_header Host $host;
          # The collector trusts this only from loopback peers, for
          # its in-memory rate limiter.
          proxy_set_header X-Forwarded-For $remote_addr;
        '';
      };

      # The maintainer's diagnose-report viewer. Auth lives HERE, not
      # in the collector: this flake auto-upgrades from a public repo,
      # so the secret can't be committed — the htpasswd file is
      # provisioned once by hand on the box:
      #
      #   install -d -m 750 -o root -g nginx /var/lib/nginx-auth
      #   printf 'jelle:%s\n' "$(openssl passwd -apr1)" \
      #     > /var/lib/nginx-auth/diagnose-htpasswd
      #   chown root:nginx /var/lib/nginx-auth/diagnose-htpasswd
      #   chmod 640 /var/lib/nginx-auth/diagnose-htpasswd
      #
      # Until that file exists nginx answers 403 for /admin/ — fail
      # closed, nothing else on the vhost is affected.
      locations."/admin/" = {
        proxyPass = "http://127.0.0.1:8787";
        basicAuthFile = "/var/lib/nginx-auth/diagnose-htpasswd";
        extraConfig = ''
          proxy_set_header Host $host;
          proxy_set_header X-Forwarded-For $remote_addr;
        '';
      };
    };
  };

  # ---- Unattended upgrades (same cadence family as origin, offset) ----
  system.autoUpgrade = {
    enable = true;
    flake = "github:cresset-tools/infra#telemetry";
    dates = "Sun 04:00";
    randomizedDelaySec = "30min";
    allowReboot = true;
    rebootWindow = { lower = "03:30"; upper = "05:30"; };
  };

  services.journald.extraConfig = ''
    SystemMaxUse=512M
    MaxRetentionSec=2week
  '';

  environment.systemPackages = with pkgs; [
    curl jq htop sqlite
  ];

  # NixOS state version — pin to the version this config was first
  # deployed against. Don't change this on upgrades.
  system.stateVersion = "25.11";
}
