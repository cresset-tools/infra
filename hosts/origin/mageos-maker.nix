# mageos-maker.cresset.tools — the Mage-OS project maker (a Laravel 13 /
# Livewire app), served by FrankenPHP in **worker mode** (Laravel Octane).
#
# Architecture: nginx already owns :443 for the static vhosts, so
# FrankenPHP can't bind it. Octane/FrankenPHP listens on loopback
# (127.0.0.1:8000) and nginx reverse-proxies the public vhost to it,
# keeping ACME/TLS centralized in nginx (matches the other vhosts in
# nginx.nix).
#
# Runtime PHP is FrankenPHP's embedded **ZTS** PHP (8.4 today; bump to a
# custom ZTS php85 build as a fast-follow — see hosts notes). The app's
# composer deps are built with pkgs.php84 (same 8.4 series).
#
# State (the SQLite DB + Laravel's writable dirs) lives in
# /var/lib/mageos-maker, outside the read-only Nix store. APP_KEY is
# generated + persisted there on first activation — the infra repo has no
# secrets framework, so nothing is committed.
#
# Hyvä private-Packagist credentials (for the catalog's Hyvä theme add-on
# versions) are read from an OPTIONAL EnvironmentFile that you provision
# on the box by hand (kept out of git since there's no secrets store):
#
#   install -o mageos-maker -g mageos-maker -m 0600 /dev/stdin \
#     /var/lib/mageos-maker/hyva.env <<'EOF'
#   MAGEOS_HYVA_PROJECT=<slug from your hyva-themes.repo.packagist.com URL>
#   MAGEOS_HYVA_LICENSE_KEY=<the http-basic "token" password>
#   EOF
#   systemctl restart mageos-maker-setup.service
#
# Without it the catalog still builds, just without Hyvä theme versions.
{ config, pkgs, lib, ... }:
let
  domain = "mageos-maker.cresset.tools";
  port = 8000;
  stateDir = "/var/lib/mageos-maker";
  user = "mageos-maker";

  php = pkgs.php84;

  # Pinned maker source — cresset-tools/mageos-maker `main` @ 2967e10
  # (#26 "Make profiles compose with the modulargento distribution; lite gets
  # very light"; builds on #25's Standard / Fully-modular toggle). Only
  # PHP/YAML/blade/test files changed since the last pin — composer.lock /
  # package-lock.json unchanged, so vendorHash + npmDepsHash below stay put —
  # only rev + hash move.
  src = pkgs.fetchFromGitHub {
    owner = "cresset-tools";
    repo = "mageos-maker";
    rev = "2967e107879c106859bed25cc7ca8ea69c717651";
    hash = "sha256-BF8UtPFHdPvRbg61NleZY24y8wpkFgNfEWt8Hma80Gg=";
  };

  # Front-end assets (public/build) via Vite, from the committed
  # package-lock.json. Hash resolved + full build verified against the
  # flake's pinned nixpkgs (549bd84).
  assets = pkgs.buildNpmPackage {
    pname = "mageos-maker-assets";
    version = "0";
    inherit src;
    npmDepsHash = "sha256-cGZUqzm7FcCQsLcKvT5iMb9EgMcxk2nbfVOlh0gFwLc=";
    npmBuildScript = "build"; # package.json "build": "vite build"
    # The Laravel app isn't an npm package; we only want the built
    # assets, not an npm `install` of a "dist".
    dontNpmInstall = true;
    installPhase = ''
      runHook preInstall
      mkdir -p "$out"
      cp -r public/build "$out/build"
      runHook postInstall
    '';
  };

  # PHP project: composer deps (vendor/) + the built assets grafted in.
  # `--no-scripts`: Laravel's post-autoload-dump (`artisan package:discover`)
  # boots the app and writes bootstrap/cache — defer it to the on-box
  # activation (migrate + package:discover + config:cache) where the DB
  # and APP_KEY exist.
  app = php.buildComposerProject {
    pname = "mageos-maker";
    version = "0";
    inherit src;
    composerNoDev = true;
    composerNoScripts = true;
    vendorHash = "sha256-yzdAZgT2aKN27jj20Kyelldi2hdLhiwTI87hGbsyQWo=";
    postInstall = ''
      cp -r ${assets}/build "$out/share/php/mageos-maker/public/build"
    '';
  };
  appRoot = "${app}/share/php/mageos-maker";

  # Runtime env. With config:cache run at activation, .env isn't read at
  # request time, but artisan still reads these during activation.
  appEnv = {
    APP_NAME = "mageos-maker";
    APP_ENV = "production";
    APP_DEBUG = "false";
    APP_URL = "https://${domain}";
    LOG_CHANNEL = "stderr";
    DB_CONNECTION = "sqlite";
    DB_DATABASE = "${stateDir}/database.sqlite";
    SESSION_DRIVER = "database";
    QUEUE_CONNECTION = "database";
    CACHE_STORE = "database";
    OCTANE_SERVER = "frankenphp";
  };
in
{
  users.users.${user} = {
    isSystemUser = true;
    group = user;
    home = stateDir;
  };
  users.groups.${user} = { };

  # Writable state lives under ${stateDir} (StateDirectory creates it):
  # the SQLite DB, the APP_KEY file, and ${stateDir}/app — a writable
  # copy of the read-only store app (Laravel needs bootstrap/cache +
  # storage writable).
  systemd.tmpfiles.rules = [
    "d ${stateDir} 0750 ${user} ${user} -"
  ];

  # One-time-per-activation provisioning: a writable app copy, APP_KEY,
  # DB migrate, and config/route/view caches. Ordered before the Octane
  # service.
  systemd.services.mageos-maker-setup = {
    description = "mageos-maker provisioning (key, migrate, caches)";
    wantedBy = [ "multi-user.target" ];
    before = [ "mageos-maker.service" ];
    path = [ php pkgs.coreutils ];
    environment = appEnv // { HOME = stateDir; };
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      User = user;
      Group = user;
      StateDirectory = "mageos-maker";
      # Hyvä private-Packagist creds for mageos:catalog:update's addon
      # version lookups (MAGEOS_HYVA_PROJECT + MAGEOS_HYVA_LICENSE_KEY).
      # A secret, so it's NOT in the Nix store/git — provision the file
      # on the box (see the module header). Optional (`-`): the catalog
      # still builds without it, just without Hyvä theme versions.
      EnvironmentFile = "-${stateDir}/hyva.env";
      # The first run bakes a graph per Mage-OS version (network-bound on
      # repo.mage-os.org); don't let systemd's start timeout kill it.
      # Subsequent runs are fast — storage/ persists the baked graphs.
      TimeoutStartSec = "30min";
    };
    script = ''
      set -euo pipefail

      # Laravel needs bootstrap/cache + storage writable, but the Nix
      # store app is read-only. Materialize a writable copy at a stable
      # path (${stateDir}/app — not the per-build store path, so cached
      # config/paths stay valid across redeploys). The SQLite DB +
      # APP_KEY live in ${stateDir} (outside the copy) so they survive
      # the re-copy on each activation. chmod -R skips symlinks, so the
      # relative vendor/bin symlinks are left intact.
      rm -rf ${stateDir}/app
      cp -rT ${appRoot} ${stateDir}/app
      chmod -R u+w ${stateDir}/app

      # storage/ is PERSISTENT (symlinked to ${stateDir}/storage), so a
      # redeploy doesn't wipe the baked Mage-OS catalog (a slow network
      # fetch against repo.mage-os.org) or logs/sessions. bootstrap/cache
      # lives in the copy — it's cheap to regenerate each activation.
      rm -rf ${stateDir}/app/storage
      ln -s ${stateDir}/storage ${stateDir}/app/storage
      mkdir -p \
        ${stateDir}/app/bootstrap/cache \
        ${stateDir}/storage/framework/cache \
        ${stateDir}/storage/framework/sessions \
        ${stateDir}/storage/framework/views \
        ${stateDir}/storage/logs \
        ${stateDir}/storage/app/private \
        ${stateDir}/storage/app/public
      cd ${stateDir}/app

      # Make Octane use OUR FrankenPHP (full extension set incl. mbstring)
      # instead of downloading its own. octane's findFrankenPhpBinary()
      # searches base_path() (= ${stateDir}/app); without this it fetches a
      # generic binary whose PHP lacks mb_split and the worker crashes.
      ln -sf ${pkgs.frankenphp}/bin/frankenphp ${stateDir}/app/frankenphp

      # APP_KEY: a base64-encoded 32-byte key (AES-256-CBC). Generated
      # from /dev/urandom rather than `artisan key:generate --show`,
      # which boots Laravel and can capture stray output (e.g. an error
      # from a half-broken earlier run) into the file. Regenerate if the
      # file is missing or not a well-formed `base64:` key.
      if ! grep -q '^base64:' ${stateDir}/app-key 2>/dev/null; then
        echo "base64:$(head -c 32 /dev/urandom | base64 -w0)" > ${stateDir}/app-key
      fi
      export APP_KEY="$(cat ${stateDir}/app-key)"

      # SQLite DB file must exist before migrate.
      [ -f ${stateDir}/database.sqlite ] || : > ${stateDir}/database.sqlite

      # Deferred post-autoload-dump work (skipped at build via
      # --no-scripts), now that the tree is writable + DB/key exist.
      php artisan package:discover --ansi
      php artisan migrate --force
      # Build the Mage-OS catalog: fetch repo.mage-os.org's packages.json
      # and pre-bake install-tree graphs into storage/. The Configurator
      # reads this; without it the UI errors with "Undefined array key".
      # Persisted in ${stateDir}/storage, so subsequent activations only
      # re-bake what changed. memory_limit raised (128M default OOMs while
      # baking the full dependency graph → uncatchable fatal, exit 255).
      php -d memory_limit=1G artisan mageos:catalog:update
      php artisan config:cache
      # route:cache fails on closure routes; view:cache is a pure
      # optimization — neither is fatal.
      php artisan route:cache || true
      php artisan view:cache || true
    '';
  };

  systemd.services.mageos-maker = {
    description = "mageos-maker (Laravel Octane / FrankenPHP worker mode)";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" "mageos-maker-setup.service" ];
    requires = [ "mageos-maker-setup.service" ];
    # Restart on every rebuild (the unit body is otherwise build-invariant,
    # so a new app/binary wouldn't otherwise relaunch the worker).
    restartTriggers = [ app pkgs.frankenphp ];
    path = [ php pkgs.frankenphp ];
    environment = appEnv // { HOME = stateDir; };
    serviceConfig = {
      Type = "simple";
      User = user;
      Group = user;
      StateDirectory = "mageos-maker";
      WorkingDirectory = "${stateDir}/app";
      # APP_KEY is a file secret; load it without baking into the unit.
      ExecStartPre = "${pkgs.coreutils}/bin/test -f ${stateDir}/app-key";
      # Run octane:start under FrankenPHP's OWN php (`php-cli`), not the
      # php84 used for the build/setup. The php84 wrapper exports
      # PHP_INI_SCAN_DIR=<php84-lib>; if octane:start runs under php84 that
      # value is inherited by the `frankenphp run` worker it spawns, whose
      # ZTS php then tries to load php84's NTS extension .so's, fails
      # silently, and ends up without mbstring → "undefined function
      # mb_split". Running under `frankenphp php-cli` keeps PHP_INI_SCAN_DIR
      # pointed at FrankenPHP's own (ZTS) extensions for both the parent
      # and the inherited worker.
      ExecStart = pkgs.writeShellScript "mageos-maker-start" ''
        export APP_KEY="$(cat ${stateDir}/app-key)"
        exec ${pkgs.frankenphp}/bin/frankenphp php-cli artisan octane:start \
          --server=frankenphp \
          --host=127.0.0.1 --port=${toString port} \
          --workers=auto --max-requests=512
      '';
      Restart = "on-failure";
      RestartSec = "2s";
      # Hardening — the app only needs its state dir writable.
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
      NoNewPrivileges = true;
    };
  };

  # Periodic catalog refresh — Mage-OS publishes new versions over time,
  # so re-fetch the manifest + re-bake graphs daily. The worker caches the
  # catalog in a process-lifetime `static`, so after refreshing the
  # on-disk catalog we recycle the worker to drop that cache. Runs as root
  # only to issue the restart; the artisan command itself is dropped to
  # the service user via runuser so storage/ writes stay user-owned.
  systemd.services.mageos-maker-catalog = {
    description = "Refresh mageos-maker's Mage-OS catalog";
    after = [ "mageos-maker-setup.service" ];
    requires = [ "mageos-maker-setup.service" ];
    serviceConfig.Type = "oneshot";
    script = ''
      set -euo pipefail
      ${pkgs.util-linux}/bin/runuser -u ${user} -- \
        env HOME=${stateDir} ${php}/bin/php -d memory_limit=1G ${stateDir}/app/artisan mageos:catalog:update
      # Recycle the worker so it re-reads the refreshed catalog (only if
      # it's currently up; a failed update above aborts before this).
      ${config.systemd.package}/bin/systemctl try-restart mageos-maker.service
    '';
  };

  systemd.timers.mageos-maker-catalog = {
    description = "Daily Mage-OS catalog refresh for mageos-maker";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "daily";
      Persistent = true; # catch up if the box was off at the scheduled time
      RandomizedDelaySec = "1h"; # spread load off the mage-os origin
    };
  };

  # Public vhost → reverse-proxy to the loopback Octane/FrankenPHP.
  # recommendedProxySettings is off globally (nginx.nix), so set the
  # proxy headers explicitly here.
  services.nginx.virtualHosts.${domain} = {
    enableACME = true;
    forceSSL = true;
    locations."/" = {
      proxyPass = "http://127.0.0.1:${toString port}";
      proxyWebsockets = true; # Livewire/Octane may upgrade
      extraConfig = ''
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_read_timeout 60s;
      '';
    };
  };
}
