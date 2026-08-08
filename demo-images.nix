# Nix-built OCI images for the licensing demo (Phase 3, see CONTAINERIZATION.md).
# Consumed by flake.nix `packages.x86_64-linux` and loaded by hosts/demo via
# `virtualisation.oci-containers … imageFile` (no registry).
#
# sconce is NOT built here. cresset-tools/sconce publishes a multi-arch, attested
# image to ghcr.io/cresset-tools/sconce on every release (its build-docker.yml),
# so rebuilding it from source was duplicated work whose only real effect was a
# hand-edited `rev`/`hash` pin. That pin is invisible to `update-flake-lock`, and
# it went stale: the console served "powered by the sconce engine" for two weeks
# after that branding was removed, because nobody bumped it. The hosts now pull
# the published tag directly — see hosts/bougierepo and hosts/demo.
{ pkgs }:
let
  lib = pkgs.lib;

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
          # Static assets are version-stamped (/static/version<ts>/…): every
          # setup:static-content:deploy bumps the version, so the URL changes and
          # busts the cache. That makes it safe to cache for a year and skip
          # revalidation entirely — previously these came back with only
          # ETag/Last-Modified, so browsers did a conditional 304 round-trip per
          # asset per page view. (Matches Magento's canonical nginx.conf.sample.)
          expires +1y;
          add_header Cache-Control "public";
          # Strip the cache-busting version prefix and serve the real file.
          location ~ ^/static/version {
            rewrite ^/static/(version\d*/)?(.*)$ /static/$2 last;
          }
          # If the asset isn't on disk, let Magento's static.php materialize it
          # (on-demand generation in default/developer mode). `resource=$2` is the
          # path after /static/[version/]. (Canonical Magento nginx.conf.sample;
          # the earlier /static/index.php target didn't exist → redirect-cycle 500.)
          # NB the rewrite re-routes to the .php location below, so a generated
          # response does NOT inherit the year-long expiry — only real on-disk
          # files do.
          if (!-f $request_filename) {
            rewrite ^/static/(version\d*/)?(.*)$ /static.php?resource=$2 last;
          }
        }
        # Media is NOT version-stamped (an image can be replaced at the same URL),
        # so cache modestly — a week — rather than a year.
        location /media/ {
          expires 7d;
          add_header Cache-Control "public";
          try_files $uri $uri/ /get.php$is_args$args;
        }
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
  # tini is PID 1 (-g: SIGTERM goes to the whole process group, since bash
  # doesn't forward signals and php-fpm/nginx are the script's children). nginx
  # only starts once php-fpm accepts on 9000 — the old `php-fpm & ; exec nginx`
  # raced on cold start. `wait -n` turns either daemon dying into a container
  # exit, so the podman unit restarts it instead of serving 502s.
  magentoEntrypoint = pkgs.writeScript "magento-entrypoint" ''
    #!${pkgs.bash}/bin/bash
    set -e
    mkdir -p /tmp/ngx-client /tmp/ngx-proxy /tmp/ngx-fastcgi /tmp/ngx-uwsgi /tmp/ngx-scgi
    ${phpRuntime}/bin/php-fpm -F -y ${magentoFpmConf} &
    ready=
    for _ in $(seq 1 50); do
      if ${phpRuntime}/bin/php -r 'exit(@fsockopen("127.0.0.1", 9000) ? 0 : 1);'; then
        ready=1; break
      fi
      sleep 0.2
    done
    if [ -z "$ready" ]; then
      echo "php-fpm not accepting on 127.0.0.1:9000 after 10s" >&2
      exit 1
    fi
    ${pkgs.nginx}/bin/nginx -g 'daemon off;' -c ${magentoNginxConf} -p /tmp &
    wait -n
    exit 1
  '';
  magentoImage = pkgs.dockerTools.buildLayeredImage {
    name = "magento";
    tag = "demo";
    contents = [ phpRuntime pkgs.nginx pkgs.bash pkgs.coreutils ];
    extraCommands = "mkdir -m 1777 -p tmp";
    config = {
      Entrypoint = [ "${pkgs.tini}/bin/tini" "-g" "--" "${magentoEntrypoint}" ];
      ExposedPorts = { "8081/tcp" = { }; };
      WorkingDir = "/var/lib/magento/current";
    };
  };
in
{
  inherit phpRuntime magentoImage;
}
