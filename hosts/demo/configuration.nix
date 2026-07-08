# Hetzner CX33 licensing-demo box (x86_64, 4 vCPU / 8 GB / 80 GB). Hosts the
# end-to-end "buy a
# cresset Composer package with a Mollie test payment, get a license key +
# install instructions, install the gated package from a private repo" demo.
#
# Two Nix-built OCI images run under rootful podman (see ../../demo-images.nix,
# exposed as inputs.self.packages.<system>.{sconceImage,magentoImage}):
#   - sconce      — the Composer v2 repo server (public, EUPL-1.2), behind
#                   repo.bougie.tools; issues/serves the licensed packages.
#   - magento     — the storefront (Mage-OS 3.1.0), behind demo.bougie.tools;
#                   the app tree itself is host state mounted in (see below).
#
# The datastores are classic NixOS services on the host, NOT containers
# (modular `system.services.*` don't cover postgres/mariadb/opensearch/redis
# at this nixpkgs pin — see CONTAINERIZATION.md). Both containers use
# `--network=host` and reach every datastore on 127.0.0.1.
#
# Secrets come from sops-nix (../secrets/demo.yaml, decrypted at activation
# with this box's SSH host key). Nothing secret is in the Nix store or git.
#
# TODOs before / at go-live (Phase 4+), deliberately not wired here yet:
#   - static IPv6 out of the box's routed /64 (added once Hetzner assigns it);
#   - restore the Magento app tree seed into /var/lib/magento/app;
#   - provision a sconce service token for the Magento module's key calls;
#   - re-apply the Mollie test key into Magento config under the new crypt key;
#   - give the magento image a real init (s6/tini) so php-fpm is ready before
#     nginx accepts (the POC entrypoint races on cold start).
{ config, pkgs, lib, inputs, ... }:
let
  # Images built by the flake's `packages.<system>` (with the rust-overlay).
  # This host is x86_64-linux, the only system those packages are built for.
  demoImages = inputs.self.packages.${pkgs.stdenv.hostPlatform.system};
in
{
  imports = [
    inputs.sops-nix.nixosModules.sops
    ../../modules/secrets.nix
  ];

  # ---- Bootloader ----
  # CX-line boots legacy BIOS; GRUB on the EF02 BIOS-boot partition from
  # disko.nix. No explicit `device` — disko derives it from the partition.
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

  # OpenSearch refuses to start below this; the JVM mmaps its indices.
  boot.kernel.sysctl."vm.max_map_count" = 262144;

  # ---- Networking ----
  # DHCP v4; static v6 out of the routed /64 with the link-local fe80::1
  # gateway (Hetzner Cloud convention — no SLAAC), same as origin/telemetry.
  networking = {
    hostName = "demo";
    domain = "bougie.tools";
    useDHCP = lib.mkDefault true;
    interfaces.enp1s0.ipv6.addresses = [
      { address = "2a01:4f8:c012:e9ea::1"; prefixLength = 64; }
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
      # 8080/8081 (containers) and the datastore ports stay loopback-only.
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

  # Owns the Magento app tree on the host; the magento container runs as this
  # uid so writes to var/, generated/, pub/static, pub/media land correctly.
  # Fixed uid because the container references it numerically (`--user`).
  users.users.magento = {
    isSystemUser = true;
    uid = 990;
    group = "magento";
    description = "Magento app-tree owner (mounted into the container)";
  };
  users.groups.magento.gid = 990;

  # ---- Secrets rendered into runtime config (sops-nix) ----
  # secrets.nix declares the raw secrets; here we compose them into the two
  # files the apps actually read: sconce's env-file and Magento's env.php.
  sops.templates."sconce-env" = {
    # podman reads this as root at container start (--env-file), so the
    # default root:0400 is fine.
    content = ''
      DATABASE_URL=postgresql://sconce:${config.sops.placeholder."postgres/sconce_password"}@127.0.0.1:5432/sconce
      SCONCE_SECRET_KEY=${config.sops.placeholder."sconce/secret_key"}
    '';
  };

  # Magento reads env.php from inside the container as uid 990; render it
  # owned by that user, and bind-mount it over the seeded app tree (below) so
  # the crypt key + DB password never live in the app-tree state on disk.
  sops.templates."magento-env-php" = {
    owner = "magento";
    mode = "0400";
    # NOTE: empty PHP values are written as "" (not '') — a literal '' would
    # collide with Nix indented-string escaping. Both are valid PHP.
    content = ''
      <?php
      return [
          'backend' => ['frontName' => 'admin'],
          'crypt' => ['key' => '${config.sops.placeholder."magento/crypt_key"}'],
          'db' => [
              'connection' => [
                  'default' => [
                      'host' => '127.0.0.1',
                      'dbname' => 'magento',
                      'username' => 'magento',
                      'password' => '${config.sops.placeholder."mariadb/magento_password"}',
                      'model' => 'mysql4',
                      'engine' => 'innodb',
                      'initStatements' => 'SET NAMES utf8;',
                      'active' => '1',
                      'driver_options' => [],
                  ],
              ],
              'table_prefix' => "",
          ],
          'resource' => ['default_setup' => ['connection' => 'default']],
          'x-frame-options' => 'SAMEORIGIN',
          'MAGE_MODE' => 'production',
          'session' => [
              'save' => 'redis',
              'redis' => [
                  'host' => '127.0.0.1',
                  'port' => '6379',
                  'password' => "",
                  'timeout' => '2.5',
                  'persistent_identifier' => "",
                  'database' => '2',
                  'compression_threshold' => '2048',
                  'compression_library' => 'gzip',
                  'log_level' => '1',
                  'max_concurrency' => '6',
                  'break_after_frontend' => '5',
                  'break_after_adminhtml' => '30',
                  'first_lifetime' => '600',
                  'bot_first_lifetime' => '60',
                  'bot_lifetime' => '7200',
                  'disable_locking' => '0',
                  'min_lifetime' => '60',
                  'max_lifetime' => '2592000',
              ],
          ],
          'cache' => [
              'frontend' => [
                  'default' => [
                      'backend' => 'Cm_Cache_Backend_Redis',
                      'backend_options' => ['server' => '127.0.0.1', 'database' => '0', 'port' => '6379'],
                  ],
                  'page_cache' => [
                      'backend' => 'Cm_Cache_Backend_Redis',
                      'backend_options' => ['server' => '127.0.0.1', 'database' => '1', 'port' => '6379', 'compress_data' => '0'],
                  ],
              ],
              'allow_parallel_generation' => false,
          ],
          'lock' => ['provider' => 'db'],
          'directories' => ['document_root_is_pub' => true],
          'cache_types' => [
              'config' => 1,
              'layout' => 1,
              'block_html' => 1,
              'collections' => 1,
              'reflection' => 1,
              'db_ddl' => 1,
              'compiled_config' => 1,
              'eav' => 1,
              'customer_notification' => 1,
              'config_integration' => 1,
              'config_integration_api' => 1,
              'graphql_query_resolver_result' => 1,
              'full_page' => 1,
              'config_webservice' => 1,
              'translate' => 1,
              'vertex' => 1,
          ],
          'install' => ['date' => 'Thu, 03 Jul 2026 00:00:00 +0000'],
      ];
    '';
  };

  # ---- Datastores (classic host services, loopback-only) ----
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

  services.mysql = {
    enable = true;
    package = pkgs.mariadb;
    settings.mysqld.bind-address = "127.0.0.1";
    ensureDatabases = [ "magento" ];
    # The 'magento'@'127.0.0.1' user needs a password for the container's TCP
    # connection, which ensureUsers (socket auth) can't set — done in the
    # demo-mysql-password oneshot below.
  };

  services.opensearch = {
    enable = true;
    # Defaults already bind 127.0.0.1:9200, single-node, security plugin off.
    extraJavaOptions = [ "-Xms1g" "-Xmx1g" ];
  };

  services.redis.servers."" = {
    enable = true;
    # Defaults: bind 127.0.0.1, port 6379.
  };

  # Apply DB passwords from sops after each datastore is up. Passwords are
  # alphanumeric (verified), so single-quoting in SQL is injection-safe.
  systemd.services.demo-postgres-password = {
    description = "Set the sconce Postgres role password from sops";
    after = [ "postgresql.service" ];
    requires = [ "postgresql.service" ];
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

  systemd.services.demo-mysql-password = {
    description = "Create/refresh the magento MariaDB user from sops";
    after = [ "mysql.service" ];
    requires = [ "mysql.service" ];
    wantedBy = [ "multi-user.target" ];
    # Runs as root: MariaDB maps OS root -> 'root'@'localhost' via unix_socket.
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      pw=$(cat ${config.sops.secrets."mariadb/magento_password".path})
      ${config.services.mysql.package}/bin/mysql -u root -e \
        "CREATE USER IF NOT EXISTS 'magento'@'127.0.0.1' IDENTIFIED BY '$pw'; \
         ALTER USER 'magento'@'127.0.0.1' IDENTIFIED BY '$pw'; \
         GRANT ALL PRIVILEGES ON magento.* TO 'magento'@'127.0.0.1'; \
         FLUSH PRIVILEGES;"
    '';
  };

  # Host-state dirs the containers mount. The app tree is populated by the
  # Phase 4 seed restore; the CAS is written by sconce (runs as root).
  systemd.tmpfiles.rules = [
    "d /var/lib/sconce      0755 root    root    -"
    "d /var/lib/sconce/cas  0755 root    root    -"
    "d /var/lib/magento     0755 magento magento -"
    "d /var/lib/magento/app 0755 magento magento -"
  ];

  # ---- Containers (rootful podman, host network) ----
  virtualisation.podman.enable = true;
  virtualisation.oci-containers = {
    backend = "podman";
    containers.sconce = {
      image = "sconce:demo";
      imageFile = demoImages.sconceImage;
      autoStart = true;
      extraOptions = [ "--network=host" ];
      volumes = [ "/var/lib/sconce/cas:/var/lib/sconce/cas" ];
      environmentFiles = [ config.sops.templates."sconce-env".path ];
    };
    containers.magento = {
      image = "magento:demo";
      imageFile = demoImages.magentoImage;
      autoStart = true;
      user = "990:990";
      extraOptions = [ "--network=host" ];
      volumes = [
        "/var/lib/magento/app:/var/www/html"
        "${config.sops.templates."magento-env-php".path}:/var/www/html/app/etc/env.php:ro"
      ];
    };
  };

  # Order the containers after their datastores + the password oneshots so the
  # apps never connect before their DB user exists. `after` on a datastore
  # unit is ordering-only (harmless if the unit name shifts); the hard
  # dependency is on the password oneshot this config owns.
  systemd.services.podman-sconce = {
    after = [ "postgresql.service" "demo-postgres-password.service" ];
    requires = [ "demo-postgres-password.service" ];
  };
  systemd.services.podman-magento = {
    after = [
      "mysql.service"
      "demo-mysql-password.service"
      "opensearch.service"
      "redis.service"
    ];
    requires = [ "demo-mysql-password.service" ];
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
    recommendedProxySettings = true; # Host + X-Forwarded-* for both upstreams

    # The Composer repo server. Composer uploads/dist can be large; sconce
    # streams from the CAS, so give it generous body headroom.
    virtualHosts."repo.bougie.tools" = {
      enableACME = true;
      forceSSL = true;
      extraConfig = "client_max_body_size 256m;";
      locations."/".proxyPass = "http://127.0.0.1:8080";
    };

    # The storefront. X-Forwarded-Proto (from recommendedProxySettings) lets
    # Magento build https URLs behind this offloader.
    virtualHosts."demo.bougie.tools" = {
      enableACME = true;
      forceSSL = true;
      extraConfig = "client_max_body_size 64m;";
      locations."/".proxyPass = "http://127.0.0.1:8081";
    };
  };

  # ---- Unattended upgrades (offset from telemetry's Sun 04:00) ----
  system.autoUpgrade = {
    enable = true;
    flake = "github:cresset-tools/infra#demo";
    dates = "Sun 05:00";
    randomizedDelaySec = "30min";
    allowReboot = true;
    rebootWindow = { lower = "04:30"; upper = "06:30"; };
  };

  services.journald.extraConfig = ''
    SystemMaxUse=1G
    MaxRetentionSec=2week
  '';

  environment.systemPackages = with pkgs; [
    curl jq htop
  ];

  # Pin to the version first deployed against; don't bump on upgrades.
  system.stateVersion = "25.11";
}
