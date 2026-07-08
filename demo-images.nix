# Nix-built OCI images for the licensing demo (Phase 3, see CONTAINERIZATION.md).
# Consumed by flake.nix `packages.x86_64-linux` and loaded by hosts/demo via
# `virtualisation.oci-containers … imageFile` (no registry).
#
# `pkgs` must carry the rust-overlay (for rustc 1.96, which nixpkgs doesn't ship
# yet — pinned from sconce's own rust-toolchain.toml).
{ pkgs }:
let
  lib = pkgs.lib;

  # ---- sconce (public repo, EUPL-1.2) — pure-rustls, tiny image ----
  sconceSrc = pkgs.fetchFromGitHub {
    owner = "cresset-tools";
    repo = "sconce";
    rev = "68ba78e51baccc427d5032fb9e5427acc54b1747";
    hash = "sha256-sOb3S/9SE0XeFiIVpt7XveOpDEIpisnJdQ0FuQp9LfA=";
  };
  rustToolchain = pkgs.rust-bin.fromRustupToolchainFile "${sconceSrc}/rust-toolchain.toml";
  rustPlatform = pkgs.makeRustPlatform { cargo = rustToolchain; rustc = rustToolchain; };
  sconce = rustPlatform.buildRustPackage {
    pname = "sconce";
    version = "0.2.0";
    src = sconceSrc;
    cargoLock.lockFile = "${sconceSrc}/Cargo.lock";
    cargoBuildFlags = [ "--bin" "sconce" ];
    doCheck = false;
  };

  sconceImage = pkgs.dockerTools.buildLayeredImage {
    name = "sconce";
    tag = "demo";
    contents = [ sconce pkgs.gitMinimal pkgs.cacert ];
    config = {
      Entrypoint = [ "${sconce}/bin/sconce" ];
      Cmd = [
        "serve" "--cas" "/var/lib/sconce/cas"
        "--listen" "0.0.0.0:8080" "--base-url" "https://repo.bougie.tools"
      ];
      Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
      Volumes = { "/var/lib/sconce/cas" = { }; };
      ExposedPorts = { "8080/tcp" = { }; };
    };
  };

  # ---- Magento runtime: php84-fpm + nginx (app tree is host state, mounted) ----
  phpRuntime = pkgs.php84.buildEnv {
    extensions = { all, enabled }: enabled ++ (with all; [
      bcmath calendar exif ftp gd gettext gmp intl opcache pcntl
      pdo_mysql redis shmop soap sockets sysvmsg sysvsem sysvshm xsl zip
    ]);
    extraConfig = ''
      memory_limit = 2G
      max_execution_time = 1800
      realpath_cache_size = 10M
      opcache.enable = 1
      opcache.memory_consumption = 512
      opcache.max_accelerated_files = 60000
    '';
  };
  magentoNginxConf = pkgs.writeText "nginx.conf" ''
    worker_processes auto;
    error_log /dev/stderr info;
    pid /tmp/nginx.pid;
    events { worker_connections 1024; }
    http {
      include ${pkgs.nginx}/conf/mime.types;
      default_type application/octet-stream;
      access_log /dev/stdout;
      client_body_temp_path /tmp/ngx-client;
      proxy_temp_path /tmp/ngx-proxy;
      fastcgi_temp_path /tmp/ngx-fastcgi;
      uwsgi_temp_path /tmp/ngx-uwsgi;
      scgi_temp_path /tmp/ngx-scgi;
      sendfile on;
      keepalive_timeout 65;
      server {
        listen 8081;
        server_name _;
        set $MAGE_ROOT /var/lib/magento/current;
        root $MAGE_ROOT/pub;
        index index.php;
        location / { try_files $uri $uri/ /index.php$is_args$args; }
        location /static/ {
          # Strip the cache-busting version prefix and serve the real file.
          location ~ ^/static/version {
            rewrite ^/static/(version\d*/)?(.*)$ /static/$2 last;
          }
          # If the asset isn't on disk, let Magento's static.php materialize it
          # (on-demand generation in default/developer mode). `resource=$2` is the
          # path after /static/[version/]. (Canonical Magento nginx.conf.sample;
          # the earlier /static/index.php target didn't exist → redirect-cycle 500.)
          if (!-f $request_filename) {
            rewrite ^/static/(version\d*/)?(.*)$ /static.php?resource=$2 last;
          }
        }
        location /media/ { try_files $uri $uri/ /get.php$is_args$args; }
        location ~ ^/(index|get|static|errors/report|errors/404|errors/503|health_check)\.php$ {
          try_files $uri =404;
          fastcgi_pass 127.0.0.1:9000;
          fastcgi_index index.php;
          fastcgi_param SCRIPT_FILENAME $realpath_root$fastcgi_script_name;
          fastcgi_read_timeout 600s;
          fastcgi_buffer_size 128k;
          fastcgi_buffers 16 128k;
          include ${pkgs.nginx}/conf/fastcgi_params;
        }
      }
    }
  '';
  magentoFpmConf = pkgs.writeText "php-fpm.conf" ''
    [global]
    error_log = /dev/stderr
    daemonize = no
    [www]
    listen = 127.0.0.1:9000
    pm = dynamic
    pm.max_children = 16
    pm.start_servers = 4
    pm.min_spare_servers = 2
    pm.max_spare_servers = 6
    clear_env = no
    catch_workers_output = yes
  '';
  # NOTE: POC entrypoint (php-fpm & ; exec nginx). Production should use a real
  # init (s6/tini) so php-fpm is ready before nginx accepts. TODO before go-live.
  magentoEntrypoint = pkgs.writeScript "magento-entrypoint" ''
    #!${pkgs.bash}/bin/bash
    set -e
    mkdir -p /tmp/ngx-client /tmp/ngx-proxy /tmp/ngx-fastcgi /tmp/ngx-uwsgi /tmp/ngx-scgi
    ${phpRuntime}/bin/php-fpm -F -y ${magentoFpmConf} &
    exec ${pkgs.nginx}/bin/nginx -g 'daemon off;' -c ${magentoNginxConf} -p /tmp
  '';
  magentoImage = pkgs.dockerTools.buildLayeredImage {
    name = "magento";
    tag = "demo";
    contents = [ phpRuntime pkgs.nginx pkgs.bash pkgs.coreutils ];
    extraCommands = "mkdir -m 1777 -p tmp";
    config = {
      Entrypoint = [ "${magentoEntrypoint}" ];
      ExposedPorts = { "8081/tcp" = { }; };
      WorkingDir = "/var/lib/magento/current";
    };
  };
in
{
  inherit sconce sconceImage phpRuntime magentoImage;
}
