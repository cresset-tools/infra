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
{ config, pkgs, lib, inputs, ... }:
let
  # Images built by the flake's `packages.<system>` (with the rust-overlay).
  # This host is x86_64-linux, the only system those packages are built for.
  demoImages = inputs.self.packages.${pkgs.stdenv.hostPlatform.system};

  # Deployer build tools: run under the SAME php the container serves with
  # (`phpRuntime`, which already carries memory_limit=2G + the full Magento
  # extension set), so di:compile/static:deploy match the runtime and composer
  # needs no --ignore-platform-reqs. Exposed on the host PATH for `dep`'s SSH tasks.
  composerPhar = pkgs.fetchurl {
    url = "https://getcomposer.org/download/2.10.2/composer.phar";
    hash = "sha256-XucSX4owo00kbO/cC8hbing7KPKuyWiZQRhRI1DSgCc=";
  };
  magentoPhp = pkgs.writeShellScriptBin "magento-php" ''
    exec ${demoImages.phpRuntime}/bin/php "$@"
  '';
  magentoComposer = pkgs.writeShellScriptBin "composer" ''
    exec ${demoImages.phpRuntime}/bin/php ${composerPhar} "$@"
  '';
  # cachetool (phar, run by magento-php) — the deploy's post-symlink opcache
  # reset, talking fastcgi to the container's php-fpm on 127.0.0.1:9000 (host
  # network). Provided here so Deployer's cachetool contrib never falls back to
  # its download-at-runtime default.
  cachetoolPhar = pkgs.fetchurl {
    url = "https://github.com/gordalina/cachetool/releases/download/10.0.0/cachetool.phar";
    hash = "sha256:07dxhclblbz4apf77q1k42fsj8xrq99sg4mm4vzflyyyrmx0xsfb";
  };
  cachetool = pkgs.runCommand "cachetool-phar" { } ''
    install -Dm644 ${cachetoolPhar} $out/bin/cachetool.phar
  '';
  # bougie (cresset-tools/bougie, public) — the deploy's package installer:
  # `bougie composer install` natively reimplements composer install (incl.
  # the two Magento install plugins) and cuts the vendors step from ~13s to
  # ~1s. The musl release build is fully static, so it runs on NixOS as-is.
  bougie = pkgs.stdenv.mkDerivation rec {
    pname = "bougie";
    version = "0.48.0";
    src = pkgs.fetchurl {
      url = "https://github.com/cresset-tools/bougie/releases/download/bougie-v${version}/bougie-x86_64-unknown-linux-musl.tar.gz";
      hash = "sha256:0p02vx0nxpfm2cjkixmfjhq1hfp6lsz9b2i9j97nv9d90wvk43qz";
    };
    sourceRoot = ".";
    dontBuild = true;
    installPhase = ''
      install -Dm755 $(find . -name bougie -type f | head -1) $out/bin/bougie
    '';
  };
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
    # Deployer builds each release over SSH as this user (uid 990 == the
    # container's runtime uid), so release/shared files are owned correctly with
    # no chown, and the php that compiles the DI is the php that serves it.
    home = "/var/lib/magento/home";
    createHome = true;
    shell = pkgs.bashInteractive;
    openssh.authorizedKeys.keys = config.users.users.root.openssh.authorizedKeys.keys ++ [
      # CI deploy key: the bougie-license-demo GitHub Action runs Deployer as
      # this user; the same key is a read-only deploy key on that repo so the
      # box's `deploy:update_code` clone works via the forwarded agent.
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIPiKvws/pN2bc6Y5AkU6TP45mRh5ew/e/UZMsfmlaEN bougie-license-demo CI deploy"
    ];
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

  # The admin UI container's env: same DB + secret key (it recovers/displays
  # license keys), plus the single-tenant basic-auth password.
  sops.templates."sconce-ui-env" = {
    content = ''
      DATABASE_URL=postgresql://sconce:${config.sops.placeholder."postgres/sconce_password"}@127.0.0.1:5432/sconce
      SCONCE_SECRET_KEY=${config.sops.placeholder."sconce/secret_key"}
      SCONCE_ADMIN_PASSWORD=${config.sops.placeholder."sconce/admin_password"}
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
          // 'production': the container now serves a Deployer release
          // (current -> releases/N) whose DI is compiled and static is fully
          // deployed by `dep deploy`, so nothing is generated on demand. This
          // env.php is linked read-only into every release; a release built in a
          // lower mode still serves correctly here because the compiled artifacts
          // are mode-independent.
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

  # Composer auth for the gated repo.bougie.tools, materialized read-only into
  # the Deployer `shared/` tree (below) so every release's auth.json resolves to
  # it. The read token is the http-basic password (sconce ignores the username).
  sops.templates."magento-auth-json" = {
    content = ''
      {
          "http-basic": {
              "repo.bougie.tools": {
                  "username": "token",
                  "password": "${config.sops.placeholder."sconce/read_token"}"
              }
          }
      }
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

  # Materialize the read-only sops files (env.php + auth.json) into the Deployer
  # `shared/` tree, so every release symlinks to them. Owned root:magento 0440 —
  # magento (build + serve) can READ but nothing can WRITE, keeping env.php
  # read-only. Re-asserts the sops-declared content on every rebuild.
  systemd.services.demo-magento-shared-seed = {
    description = "Seed Magento shared/ from sops (env.php, auth.json)";
    wantedBy = [ "multi-user.target" ];
    before = [ "podman-magento.service" ];
    # The rendered sops paths are stable, so a content-only change (e.g. flipping
    # MAGE_MODE) wouldn't otherwise restart this oneshot and shared/ would keep the
    # stale file. Re-run whenever the rendered content changes.
    restartTriggers = [
      config.sops.templates."magento-env-php".content
      config.sops.templates."magento-auth-json".content
    ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      install -D -o root -g magento -m 0440 \
        ${config.sops.templates."magento-env-php".path} \
        /var/lib/magento/shared/app/etc/env.php
      install -D -o root -g magento -m 0440 \
        ${config.sops.templates."magento-auth-json".path} \
        /var/lib/magento/shared/auth.json
    '';
  };

  # Magento cron, run against the current release with the same php that serves
  # it. This is not optional plumbing: indexers, scheduled jobs, AND the MysqlMq
  # message-queue consumers all hang off cron — the Mollie module's webhook
  # controller only *queues* transactions (payment/mollie_general queue mode),
  # so without cron a paid order sits in pending_payment forever. cron:run also
  # spawns the consumers_runner each minute (env.php has no cron_consumers_runner
  # override → Magento default: enabled).
  systemd.services.magento-cron = {
    description = "Magento cron:run (current release)";
    serviceConfig = {
      Type = "oneshot";
      User = "magento";
      Group = "magento";
      WorkingDirectory = "/var/lib/magento/current";
      ExecStart = "/run/current-system/sw/bin/magento-php /var/lib/magento/current/bin/magento cron:run";
    };
    # No release deployed yet (fresh box) -> skip instead of failing the unit.
    unitConfig.ConditionPathExists = "/var/lib/magento/current/bin/magento";
  };
  systemd.timers.magento-cron = {
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "minutely";
      # No catch-up storm after downtime; cron_schedule reconciles itself.
      Persistent = false;
    };
  };

  # The payment-critical queue consumer, supervised. Magento's own
  # consumers_runner (the `consumers` cron group) spawns this too, but that
  # spawner keeps its bookkeeping in the Magento cache — a cache:flush (every
  # deploy!) stalled it for ~10 minutes, which for the Mollie webhook queue
  # means paid orders sitting in pending_payment. Run it under systemd as well:
  # --single-thread takes a per-consumer lock, so whichever instance starts
  # first wins and the other exits — no double consumption. max-messages +
  # RuntimeMaxSec recycle the process so it never runs a stale release's code
  # for long after a deploy (the symlinked release path is resolved at spawn).
  systemd.services.magento-consumer-mollie = {
    description = "Magento queue consumer: mollie.transaction.processor";
    wantedBy = [ "multi-user.target" ];
    after = [ "mysql.service" ];
    serviceConfig = {
      User = "magento";
      Group = "magento";
      WorkingDirectory = "/var/lib/magento/current";
      ExecStart = "/run/current-system/sw/bin/magento-php /var/lib/magento/current/bin/magento queue:consumers:start mollie.transaction.processor --single-thread --max-messages=1000";
      Restart = "always";
      # Calm restarts while a Magento-spawned twin holds the single-thread lock.
      RestartSec = 60;
      RuntimeMaxSec = 900;
    };
    unitConfig.ConditionPathExists = "/var/lib/magento/current/bin/magento";
  };

  # Deploys pause the Magento cron machinery around the release swap (no new
  # cron:run against a half-swapped tree; the queue consumer restarts onto the
  # new release immediately instead of riding out RuntimeMaxSec on stale code).
  # Deployer runs as `magento`, so authorize exactly those unit operations via
  # polkit — scoped to the three magento units and the three verbs, nothing
  # else (deliberately not sudo: no shell, no argv surface, just D-Bus).
  security.polkit.enable = true;
  security.polkit.extraConfig = ''
    polkit.addRule(function (action, subject) {
      if (action.id == "org.freedesktop.systemd1.manage-units" &&
          subject.user == "magento") {
        var unit = action.lookup("unit");
        var verb = action.lookup("verb");
        if ((unit == "magento-cron.timer" ||
             unit == "magento-cron.service" ||
             unit == "magento-consumer-mollie.service") &&
            (verb == "start" || verb == "stop" || verb == "restart")) {
          return polkit.Result.YES;
        }
      }
    });
  '';

  # Host-state dirs for the Deployer atomic-release layout (`releases/`+`shared/`
  # + `current` symlink), owned by the magento build/serve user. The container
  # mounts the whole /var/lib/magento and serves `current`.
  systemd.tmpfiles.rules = [
    "d /var/lib/sconce                 0755 root    root    -"
    "d /var/lib/sconce/cas             0755 root    root    -"
    "d /var/lib/magento                0755 magento magento -"
    "d /var/lib/magento/releases       0755 magento magento -"
    "d /var/lib/magento/shared         0755 magento magento -"
    "d /var/lib/magento/shared/app     0755 magento magento -"
    "d /var/lib/magento/shared/app/etc 0755 magento magento -"
    # cachetool's fcgi temp files: must be visible to the container's php-fpm
    # at the same path (the whole /var/lib/magento is mounted identically).
    "d /var/lib/magento/tmp            0755 magento magento -"
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
    # The operator dashboard (admin.bougie.tools). Same image as `sconce`, but
    # running the `ui` listener in single-tenant mode: no user accounts, gated
    # by HTTP basic auth (any username; the sops admin_password). Loopback-only
    # — nginx terminates TLS and proxies to it.
    containers.sconce-ui = {
      image = "sconce:demo";
      imageFile = demoImages.sconceImage;
      autoStart = true;
      extraOptions = [ "--network=host" ];
      cmd = [
        "ui"
        "--single-tenant"
        "--listen"
        "127.0.0.1:8082"
        "--public-base-url"
        "https://repo.bougie.tools"
      ];
      environmentFiles = [ config.sops.templates."sconce-ui-env".path ];
    };
    containers.magento = {
      image = "magento:demo";
      imageFile = demoImages.magentoImage;
      autoStart = true;
      user = "990:990";
      extraOptions = [ "--network=host" ];
      # Mount the whole Deployer tree at the identical path so atomic symlink
      # swaps (current -> releases/N) are honored live and absolute symlinks
      # (env.php, auth.json, shared dirs) resolve the same inside the container.
      # env.php is the release's read-only symlink into shared/, so no separate
      # bind-mount is needed.
      volumes = [ "/var/lib/magento:/var/lib/magento" ];
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
  systemd.services.podman-sconce-ui = {
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

    # The sconce operator dashboard (single-tenant UI container). App-level
    # HTTP basic auth (SCONCE_ADMIN_PASSWORD) on top of TLS.
    virtualHosts."admin.bougie.tools" = {
      enableACME = true;
      forceSSL = true;
      locations."/".proxyPass = "http://127.0.0.1:8082";
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
    # Deployer server-side toolchain (used over SSH as the magento user):
    # `magento-php` + `composer` run under the container's phpRuntime (kept for
    # fallback box builds and one-off bin/magento runs — releases normally
    # arrive as CI-built artifacts); redis for the deploy's old-cache-namespace
    # prune (redis-cli scan/unlink of the previous release's id_prefix keys).
    magentoPhp magentoComposer unzip git bougie cachetool redis
  ];

  # Trust github.com for the magento user's `git clone` in deploy:update_code
  # (Deployer 6 clones without a StrictHostKeyChecking override, so an unknown
  # host key would abort the deploy non-interactively).
  programs.ssh.knownHosts = {
    "github.com".publicKey =
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
  };

  # Pin to the version first deployed against; don't bump on upgrades.
  system.stateVersion = "25.11";
}
