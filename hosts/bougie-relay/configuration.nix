# Hetzner CX23 relay box (x86_64). Runs bougie-relay — the public rendezvous
# for `bougie share`: it terminates HTTPS for *.bougie.show and reverse-
# proxies each inbound request down a laptop's outbound tunnel (see
# cresset-tools/bougie-relay).
#
# Its own host on purpose. A public reverse-proxy for untrusted laptop tunnels
# is an abuse surface that must not share blast radius with the release mirror
# or telemetry — the same reasoning as hosts/telemetry, and the same reason
# bougie.show is a separate registrable domain from bougie.run / bougie.tools.
#
# Unlike every other host in this fleet, the relay does NOT sit behind nginx:
# it terminates TLS itself (the yamux tunnel + the hyper reverse proxy need the
# raw stream), so it reads a *wildcard* *.bougie.show cert straight from
# security.acme. That wildcard forces ACME DNS-01 (HTTP-01 can't issue
# wildcards) — the only DNS-01 cert in this fleet; its Cloudflare token comes
# from sops (../secrets/relay.yaml).
#
# Everything arrives on :443: viewers on `<slug>.bougie.show`, laptops on
# `tunnel.bougie.show`, told apart by SNI (a separate tunnel port was blocked
# by networks that only let HTTPS out). Anyone may VIEW a share; CREATING one
# needs a valid `bougie login` token (introspected against sconce) AND a source
# address in tunnelAllowFrom, which the relay itself enforces.
#
# NB provisioned as x86/cx23 (Hetzner ARM/CAX was capacity-unavailable at
# launch); the relay is arch-agnostic.
{ config, pkgs, lib, inputs, ... }:
let
  # Build the (private) relay straight from its repo, pinned by flake.lock.
  # Not vendored into this public repo — the relay stays closed source.
  bougie-relay = pkgs.rustPlatform.buildRustPackage {
    pname = "bougie-relay";
    version = "0.1.0";
    src = inputs.bougie-relay;
    cargoLock.lockFile = inputs.bougie-relay + "/Cargo.lock";
  };

  # Which source addresses may open tunnels (i.e. run `bougie share`), as IPs
  # or CIDRs; enforced in the relay, which reports a refused client's address
  # back to it. tunnel.bougie.show is dual-stack, so list your IPv6 prefix as
  # well as your IPv4. Re-deploy when they move.
  # (62.132.41.81 = the operator's home NAT: dev-debn-02 and the Mac.)
  tunnelAllowFrom = [ "62.132.41.81" ];

  # Where security.acme (DNS-01) writes the wildcard cert.
  certDir = "/var/lib/acme/bougie.show";
in
{
  imports = [
    inputs.sops-nix.nixosModules.sops
    ../../modules/ci-deploy-key.nix
  ];

  # ---- Bootloader (CX23 boots legacy BIOS; mirror hosts/telemetry) ----
  # GRUB with the EF02 BIOS-boot partition from disko.nix; disko derives the
  # install device, so no explicit boot.loader.grub.device.
  boot.loader.grub.enable = true;
  boot.kernelParams = [ "console=tty1" "console=ttyS0,115200" ];
  boot.initrd.availableKernelModules = [
    "virtio_pci" "virtio_scsi" "virtio_blk" "virtio_net" "ahci" "xhci_pci" "sd_mod" "sr_mod"
  ];

  # ---- Networking ----
  networking = {
    hostName = "bougie-relay";
    domain = "bougie.show";
    useDHCP = lib.mkDefault true;
    interfaces.enp1s0.ipv6.addresses = [
      { address = "2a01:4f8:c17:eb50::1"; prefixLength = 64; }
    ];
    defaultGateway6 = { address = "fe80::1"; interface = "enp1s0"; };
    firewall = {
      enable = true;
      # 443 open to the world (share viewers and tunnels, dual-stack). SSH for
      # the operator.
      allowedTCPPorts = [ 22 443 ];
    };
  };

  # ---- Time + locale ----
  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  # ---- SSH (same operator keys as the rest of the fleet) ----
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

  # ---- Secrets (sops-nix, like hosts/demo) ----
  # The Cloudflare DNS-01 API token lives in ../secrets/relay.yaml, decrypted
  # at activation with the box's own SSH host key (planted at provision time
  # via `nixos-anywhere --extra-files`). Admins edit via the age recipients in
  # ../.sops.yaml (dev-debn-02 + the MacBook).
  sops.defaultSopsFile = ../../secrets/relay.yaml;
  sops.age.sshKeyPaths = [ "/etc/ssh/ssh_host_ed25519_key" ];
  sops.secrets."cloudflare_dns_token" = { };
  # The shared secret the relay presents to sconce's /oauth/introspect — must
  # equal bougierepo's SCONCE_INTROSPECT_SECRET (the same value lives in both
  # secrets/relay.yaml and secrets/bougierepo.yaml).
  sops.secrets."introspect_secret" = { };
  # The lego-style env file the ACME DNS-01 (Cloudflare) provider reads.
  sops.templates."acme-cloudflare.env".content =
    "CF_DNS_API_TOKEN=${config.sops.placeholder."cloudflare_dns_token"}\n";
  # The relay's own secret env-file (loaded by the service below via
  # EnvironmentFile, so the secret never lands in the Nix store).
  sops.templates."relay.env".content =
    "BOUGIE_RELAY_INTROSPECT_SECRET=${config.sops.placeholder."introspect_secret"}\n";

  # ---- Wildcard TLS via ACME DNS-01 (Cloudflare) ----
  # HTTP-01 can't issue wildcards, so this is the fleet's only DNS-01 cert.
  security.acme = {
    acceptTerms = true;
    defaults.email = "jelle@pingiun.com";
    certs."bougie.show" = {
      domain = "bougie.show";
      extraDomainNames = [ "*.bougie.show" ];
      dnsProvider = "cloudflare";
      environmentFile = config.sops.templates."acme-cloudflare.env".path;
      # Give the relay's (dynamic) user read access to the key, and bounce it
      # whenever the cert renews.
      group = "acme-relay";
      reloadServices = [ "bougie-relay.service" ];
    };
  };
  users.groups.acme-relay = { };

  # ---- The relay ----
  systemd.services.bougie-relay = {
    description = "bougie share public rendezvous relay";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" "acme-finished-bougie.show.target" ];
    wants = [ "acme-finished-bougie.show.target" ];
    serviceConfig = {
      ExecStart = "${bougie-relay}/bin/bougie-relay";
      DynamicUser = true;
      # Read the wildcard key (mode 640, group acme-relay).
      SupplementaryGroups = [ "acme-relay" ];
      # Secret env-file (sops): BOUGIE_RELAY_INTROSPECT_SECRET.
      EnvironmentFile = [ config.sops.templates."relay.env".path ];
      Environment = [
        "BOUGIE_RELAY_DOMAIN=bougie.show"
        # Dual-stack; viewers and tunnels both, routed by SNI.
        "BOUGIE_RELAY_PUBLIC_ADDR=[::]:443"
        "BOUGIE_RELAY_TUNNEL_HOST=tunnel.bougie.show"
        "BOUGIE_RELAY_TUNNEL_ALLOW_FROM=${lib.concatStringsSep "," tunnelAllowFrom}"
        "BOUGIE_RELAY_CERT=${certDir}/fullchain.pem"
        "BOUGIE_RELAY_KEY=${certDir}/key.pem"
        # Share *creation* now requires a valid `bougie login` token: the relay
        # introspects it against the production identity at app.bougie.cloud (the
        # Bougie Cloud console — sconce's /oauth/introspect; the secret comes
        # from EnvironmentFile above). The tunnelAllowFrom check stays as a
        # second layer.
        "BOUGIE_RELAY_SCONCE_URL=https://app.bougie.cloud"
        "RUST_LOG=info"
      ];
      Restart = "always";
      RestartSec = 2;
      # Non-root but must bind :443.
      AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
      CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
      # Hardening — mirror the telemetry collector; relax only what a public
      # network listener on :443 needs.
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
    };
  };

  # ---- Unattended upgrades ----
  # DISABLED for now: the relay is built from a PRIVATE flake input
  # (cresset-tools/bougie-relay, distributed via FlakeHub), so an autoUpgrade
  # that refetches infra would need this box authenticated to FlakeHub — a
  # FlakeHub token wired into nix's netrc (e.g. a sops-templated netrc, or
  # `determinate-nixd login token` at provision time). Until that's set up,
  # update by hand (uses the operator's own `determinate-nixd login`):
  #   nix run .#switch -- bougie-relay <ip>
  system.autoUpgrade = {
    enable = false;
    flake = "github:cresset-tools/infra#bougie-relay";
    dates = "Sun 05:00";
    randomizedDelaySec = "30min";
    allowReboot = true;
    rebootWindow = { lower = "04:30"; upper = "06:30"; };
  };

  services.journald.extraConfig = ''
    SystemMaxUse=512M
    MaxRetentionSec=2week
  '';

  environment.systemPackages = with pkgs; [ curl jq htop ];

  # Pin to the version first deployed against; don't bump on upgrades.
  system.stateVersion = "25.11";
}
