# Hetzner cpx22 bougierepo box (x86_64, AMD). Runs sconce — the Composer v2
# repo/registry server (public, EUPL-1.2) — as the production registry behind
# https://bougierepo.com. This is the official hosted sconce that the relay
# introspects share-creation tokens against (see below).
#
# It is essentially hosts/demo minus Magento and minus the mariadb/opensearch/
# redis datastores: just postgres + the sconce API container + nginx + sops.
# The one net-new piece is SCONCE_INTROSPECT_SECRET — the bougie-relay's
# `/oauth/introspect` calls into sconce are fail-closed unless sconce is told
# this shared secret, so it is rendered into the sconce env alongside the
# secret key.
#
# The sconce OCI image is the one cresset-tools/sconce publishes to GHCR on every
# release (multi-arch, provenance-attested), pulled by the rolling `latest` tag
# and run under rootful podman with --network=host, reaching postgres on
# 127.0.0.1. nginx terminates TLS and proxies to the sconce API on 127.0.0.1:8080.
#
# Deliberately NOT pinned here: this used to be a Nix build off a hand-written
# `rev`/`hash` in ../../demo-images.nix, which `update-flake-lock` cannot see, so
# it silently rotted (the console served branding that had been removed two weeks
# earlier). Releasing sconce is now the whole deploy: release-please cuts the tag,
# build-docker.yml publishes it, and this host picks it up on the next restart of
# podman-sconce.service. Rollback = point `image` at an explicit version tag.
#
# Secrets come from sops-nix (../../secrets/bougierepo.yaml, decrypted at
# activation with this box's SSH host key). Nothing secret lands in the Nix
# store or git.
{ config, pkgs, lib, inputs, ... }:
{
  imports = [
    inputs.sops-nix.nixosModules.sops
    ../../modules/ci-deploy-key.nix
  ];

  # ---- Bootloader ----
  # This cpx (AMD) box boots UEFI (verified via /sys/firmware/efi), unlike the
  # legacy-BIOS cx/ccx fleet — so systemd-boot on the ESP from disko.nix, not
  # GRUB on an EF02 stage (which left the UEFI firmware nothing to boot).
  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;

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
  # DHCP v4; static v6 out of the routed /64 with the link-local fe80::1
  # gateway (Hetzner Cloud convention — no SLAAC), same as the rest of the fleet.
  networking = {
    hostName = "bougierepo";
    # This cpx (AMD) box presents its NIC as `eth0` (verified on the box),
    # unlike the Intel cx-line fleet that comes up as `enp1s0`. Disable
    # predictable names so the interface is deterministically `eth0` regardless
    # of PCI topology — otherwise the static v6 + gateway land on a ghost
    # interface and the box comes up with no network.
    usePredictableInterfaceNames = false;
    useDHCP = lib.mkDefault true;
    interfaces.eth0.ipv6.addresses = [
      { address = "2a01:4f8:c015:db22::1"; prefixLength = 64; }
    ];
    defaultGateway6 = {
      address = "fe80::1";
      interface = "eth0";
    };
    firewall = {
      enable = true;
      allowedTCPPorts = [
        22   # SSH
        80   # ACME HTTP-01 + HTTPS redirect
        443  # HTTPS
      ];
      # 8080 (sconce API) and postgres stay loopback-only.
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

  # ---- Secrets (sops-nix) ----
  # Decrypted at activation with the box's own SSH host key (planted at provision
  # time via `nixos-anywhere --extra-files`). Admins edit via the age recipients
  # in ../../.sops.yaml. modules/secrets.nix is demo-specific (hard-codes
  # secrets/demo.yaml + the mariadb/magento/mollie secrets), so this host wires
  # sops inline, mirroring hosts/bougie-relay, pointing at its own secrets file.
  sops.defaultSopsFile = ../../secrets/bougierepo.yaml;
  sops.age.sshKeyPaths = [ "/etc/ssh/ssh_host_ed25519_key" ];

  # postgres/sconce_password is also read directly by the bougierepo-postgres-
  # password oneshot below, which runs as the `postgres` user (peer auth) — so
  # it must own the file, otherwise `cat` hits Permission denied on the default
  # root:0400.
  sops.secrets = {
    "postgres/sconce_password" = { owner = "postgres"; };
    "sconce/secret_key" = { };
    "sconce/introspect_secret" = { };
  };

  # Compose the raw secrets into the env-file the sconce container reads.
  # podman reads this as root at container start (--env-file), so the default
  # root:0400 is fine.
  sops.templates."sconce-env" = {
    content = ''
      DATABASE_URL=postgresql://sconce:${config.sops.placeholder."postgres/sconce_password"}@127.0.0.1:5432/sconce
      SCONCE_SECRET_KEY=${config.sops.placeholder."sconce/secret_key"}
      SCONCE_INTROSPECT_SECRET=${config.sops.placeholder."sconce/introspect_secret"}
    '';
  };

  # ---- Datastore (classic host service, loopback-only) ----
  services.postgresql = {
    enable = true;
    package = pkgs.postgresql_17;
    settings.listen_addresses = lib.mkForce "127.0.0.1";
    ensureDatabases = [ "sconce" ];
    ensureUsers = [
      { name = "sconce"; ensureDBOwnership = true; }
    ];
    # Peer auth over the socket (setup + the password oneshot); scram over
    # loopback TCP for the container. pg17 encrypts passwords scram by default.
    authentication = lib.mkAfter ''
      host  sconce  sconce  127.0.0.1/32  scram-sha-256
      host  sconce  sconce  ::1/128       scram-sha-256
    '';
  };

  # Apply the sconce DB password from sops after postgres is up. The password is
  # alphanumeric (verified), so single-quoting in SQL is injection-safe.
  systemd.services.bougierepo-postgres-password = {
    description = "Set the sconce Postgres role password from sops";
    # The `sconce` role + DB are created by postgresql-setup.service (a separate
    # unit from postgresql.service), so order after THAT — otherwise this races
    # ahead and fails with "role sconce does not exist".
    after = [ "postgresql.service" "postgresql-setup.service" ];
    requires = [ "postgresql.service" "postgresql-setup.service" ];
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      Type = "oneshot";
      User = "postgres";
      RemainAfterExit = true;
    };
    script = ''
      pw=$(cat ${config.sops.secrets."postgres/sconce_password".path})
      ${config.services.postgresql.package}/bin/psql -v ON_ERROR_STOP=1 --no-psqlrc \
        -c "ALTER ROLE sconce WITH LOGIN PASSWORD '$pw';"
    '';
  };

  # Host-state dir for sconce's content-addressed store (CAS), mounted into the
  # container.
  # The published image runs as uid 10001 (`useradd --system --uid 10001 sconce`
  # in its Dockerfile); the Nix-built image it replaces ran as root. Under
  # ROOTFUL podman that uid is the host uid, so a root-owned CAS would leave
  # sconce unable to write a single object. `Z` recursively re-owns what the
  # previous image already wrote there as root — plain `d` only applies to the
  # directory it creates, which would have left the existing CAS unreadable.
  systemd.tmpfiles.rules = [
    "d /var/lib/sconce      0755 root  root  -"
    "d /var/lib/sconce/cas  0755 10001 10001 -"
    "Z /var/lib/sconce/cas  0755 10001 10001 -"
  ];

  # ---- Container (rootful podman, host network) ----
  virtualisation.podman.enable = true;
  virtualisation.oci-containers = {
    backend = "podman";
    containers.sconce = {
      image = "ghcr.io/cresset-tools/sconce:latest";
      # Re-resolve the rolling tag on every service start, so restarting the
      # unit (or rebooting the box) is what rolls a new sconce release out.
      # Without this podman keeps whatever it pulled first and `latest` would
      # be a lie after the first start.
      #
      # `newer`, not `always`: both pick up a new release, but `always` makes a
      # reachable GHCR a hard precondition for starting — a registry outage or
      # a network blip during a reboot would leave the box's own registry down
      # with a perfectly good image sitting in local storage. `newer` pulls when
      # it can and falls back to that local image when it cannot.
      pull = "newer";
      autoStart = true;
      extraOptions = [ "--network=host" ];
      # Registry/identity split (brand: registry = bougierepo.com, identity /
      # console = bougie.cloud). One sconce process: the Composer wire server on
      # :8080 (`--base-url` = the registry origin — drives packages.json + dist
      # URLs and the repo URLs `bougie login` writes into projects) plus the
      # dashboard UI on :8081 (`--ui-listen`). `SCONCE_UI_BASE_URL` (in
      # sconce-env) points the UI self-links + the device-flow verification URL
      # at bougie.cloud. Both bind loopback-only; nginx fans the two vhosts in.
      # (Overrides the image default `--base-url https://repo.bougie.tools`.)
      cmd = [
        "serve"
        "--cas" "/var/lib/sconce/cas"
        "--listen" "127.0.0.1:8080"
        "--ui-listen" "127.0.0.1:8081"
        "--base-url" "https://bougierepo.com"
      ];
      volumes = [ "/var/lib/sconce/cas:/var/lib/sconce/cas" ];
      environmentFiles = [ config.sops.templates."sconce-env".path ];
      # Identity/console origin (not secret): the UI's self-links + the
      # device-flow verification URL resolve to app.bougie.cloud (the console
      # subdomain; the bougie.cloud apex is the marketing landing), while
      # --base-url keeps registry/dist URLs on bougierepo.com.
      environment.SCONCE_UI_BASE_URL = "https://app.bougie.cloud";
    };
  };

  # Order the container after postgres + the password oneshot so sconce never
  # connects before its DB user exists. `after` on postgresql is ordering-only;
  # the hard dependency is on the password oneshot this config owns.
  systemd.services.podman-sconce = {
    after = [ "postgresql.service" "bougierepo-postgres-password.service" ];
    requires = [ "bougierepo-postgres-password.service" ];
  };

  # ---- nginx: TLS termination + reverse proxy ----
  security.acme = {
    acceptTerms = true;
    defaults.email = "jelle@pingiun.com";
  };

  services.nginx = {
    enable = true;
    recommendedOptimisation = true;
    recommendedTlsSettings = true;
    recommendedGzipSettings = true;
    recommendedProxySettings = true; # Host + X-Forwarded-* to the upstream

    # The Composer repo server. Composer uploads/dist can be large; sconce
    # streams from the CAS, so give it generous body headroom.
    virtualHosts."bougierepo.com" = {
      enableACME = true;
      forceSSL = true;
      extraConfig = "client_max_body_size 256m;";
      # A human hitting the bare registry root gets the marketing landing; every
      # real registry path (/{org}/{repo}/…, /api, /oauth, /dist, /p2) stays on
      # sconce — the exact-match `= /` only catches the apex.
      locations."= /".return = "302 https://bougie.cloud";
      locations."/".proxyPass = "http://127.0.0.1:8080";
    };

    # Apex bougie.cloud = the marketing landing (static, served from ./landing).
    virtualHosts."bougie.cloud" = {
      enableACME = true;
      forceSSL = true;
      root = ./landing;
    };

    # The console (app.bougie.cloud): OAuth API (device, device/token,
    # introspect — what `bougie login` + the relay hit) on the wire server
    # :8080; the device-approval page, dashboard, login, and assets on the UI
    # :8081. Same sconce process, different listeners.
    virtualHosts."app.bougie.cloud" = {
      enableACME = true;
      forceSSL = true;
      locations."/oauth/".proxyPass = "http://127.0.0.1:8080";
      locations."/".proxyPass = "http://127.0.0.1:8081";
    };
  };

  # ---- Unattended upgrades (offset from demo's Sun 05:00) ----
  system.autoUpgrade = {
    enable = true;
    flake = "github:cresset-tools/infra#bougierepo";
    dates = "Sun 06:00";
    randomizedDelaySec = "30min";
  };

  services.journald.extraConfig = ''
    SystemMaxUse=1G
    MaxRetentionSec=2week
  '';

  environment.systemPackages = with pkgs; [ curl jq htop ];

  # Pin to the version first deployed against; don't bump on upgrades.
  system.stateVersion = "25.11";
}
