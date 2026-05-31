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
{ config, pkgs, lib, ... }:
let
  domain = "mageos-maker.cresset.tools";
  port = 8000;
  stateDir = "/var/lib/mageos-maker";
  user = "mageos-maker";

  php = pkgs.php84;

  # Pinned maker source — master merge commit of cresset-tools/mageos-maker#1
  # (adds laravel/octane + the package-lock.json the reproducible build needs).
  src = pkgs.fetchFromGitHub {
    owner = "cresset-tools";
    repo = "mageos-maker";
    rev = "4bd39383237964603ba0c1bc686712857c946f96";
    hash = "sha256-8UAn3UMA6bPegH60qc8gg5KhVK6Zx/Q1eMG7eZuaRC4=";
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

  # Writable state: SQLite DB + Laravel's storage/ and bootstrap/cache.
  # The store app symlinks these out to here (done in the activation
  # oneshot below, since the store paths are read-only).
  systemd.tmpfiles.rules = [
    "d ${stateDir} 0750 ${user} ${user} -"
    "d ${stateDir}/storage 0750 ${user} ${user} -"
    "d ${stateDir}/bootstrap-cache 0750 ${user} ${user} -"
  ];

  # One-time-per-activation provisioning: APP_KEY, a writable app tree
  # (store app + symlinked state), DB migrate, and config/route/view
  # caches. Ordered before the Octane service.
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
    };
    script = ''
      set -euo pipefail
      cd ${appRoot}

      # APP_KEY: generate once, persist outside the store.
      if [ ! -f ${stateDir}/app-key ]; then
        php artisan key:generate --show > ${stateDir}/app-key
      fi
      export APP_KEY="$(cat ${stateDir}/app-key)"

      # SQLite DB file must exist before migrate.
      [ -f ${stateDir}/database.sqlite ] || : > ${stateDir}/database.sqlite

      # The store app is read-only; Laravel needs writable storage/ +
      # bootstrap/cache. artisan is run with config pointing at the
      # store app, but writes (cache, sessions, logs) go to state via
      # env + the symlinks the service sets up. Run the deferred
      # post-autoload-dump work here where DB + key exist.
      php artisan package:discover --ansi || true
      php artisan migrate --force
      php artisan config:cache
      php artisan route:cache || true
      php artisan view:cache || true
    '';
  };

  systemd.services.mageos-maker = {
    description = "mageos-maker (Laravel Octane / FrankenPHP worker mode)";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" "mageos-maker-setup.service" ];
    requires = [ "mageos-maker-setup.service" ];
    path = [ php pkgs.frankenphp ];
    environment = appEnv // { HOME = stateDir; };
    serviceConfig = {
      Type = "simple";
      User = user;
      Group = user;
      StateDirectory = "mageos-maker";
      WorkingDirectory = appRoot;
      # APP_KEY is a file secret; load it without baking into the unit.
      ExecStartPre = "${pkgs.coreutils}/bin/test -f ${stateDir}/app-key";
      ExecStart = pkgs.writeShellScript "mageos-maker-start" ''
        export APP_KEY="$(cat ${stateDir}/app-key)"
        exec ${php}/bin/php artisan octane:start \
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
