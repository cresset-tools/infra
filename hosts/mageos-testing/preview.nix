{ config, pkgs, lib, ... }:

# Preview environments (trial): a branch of a Magento shop on its own URL with
# its own masked database, each in a microvm (./preview-vm.nix).
#
#   <name>.preview.cresset.tools ─ nginx ─ 127.0.0.1:81<slot> ─ VM slot ─ /data
#                                                                  │
#                                     ZFS clone of preview/template ┘
#
# The data lives in a ZFS pool kept in a file on /srv (the disk is one ext4
# partition; a file vdev needs no repartitioning). A preview's /data is a clone
# of the template dataset, which takes a tenth of a second whatever its size,
# and the pool has sync=disabled: an 800-table schema took 85 s to create with
# fsync and 0.5 s without.
#
# Slots are declared here: a fixed number of identical VMs that run only
# while a preview is assigned. The guest boots straight into a minimal systemd
# (see ./preview-vm.nix), so a cold start is short enough not to need a pool
# of waiting VMs.
#
# `preview` (../../tools/preview, Rust) does the rest. `preview up <name>`
# clones the template dataset, copies the template database, renders the
# slot's env.php, starts the slot's taper and VM and adds the route; `preview
# down` removes all of it. `preview serve` takes GitHub's push webhook and
# builds and deploys the pushed commit with magebuild into the branch's
# preview; `preview exec` and `preview ssh` run commands in a guest through
# its agent on vsock, and `magebuild deploy --ssh "preview ssh"` deploys over
# exactly that.
#
# Each slot's database is PostgreSQL behind its own taper (MySQL, Redis and
# the search API in one process), all on this host, so a guest runs only
# nginx and php-fpm. Neither is from nixpkgs yet, and for the trial both are
# placed by hand: taper is not on main, and the PostgreSQL it needs carries
# two extensions of taper's own.
#
#   /var/lib/preview/bin/taper     a static build of taper, at a pinned revision
#   /var/lib/preview/postgresql    a symlink to taper's postgresql-shade build
#
# Also by hand, for building and the webhook: /var/lib/preview/deploy_key (a
# read-only deploy key for the shop's repository), /var/lib/preview/auth.json
# (Composer's credentials for it) and /var/lib/preview/webhook-secret.
let
  slots = [ 1 2 3 ];
  domain = "preview.cresset.tools";
  state = "/srv/preview";
  tls = "/var/lib/preview/tls";

  preview = import ../../tools/preview/package.nix { inherit pkgs; };

  magebuild = pkgs.stdenvNoCC.mkDerivation rec {
    pname = "magebuild";
    version = "0.18.1";
    src = pkgs.fetchurl {
      url = "https://github.com/cresset-tools/magebuild/releases/download/magebuild-v${version}/magebuild-x86_64-unknown-linux-musl.tar.gz";
      hash = "sha256:10m07n0hmxsk5qkmj17zlk2m8srwqg0a1i0xadic139hgw4gj6g1";
    };
    dontBuild = true;
    installPhase = "install -Dm755 magebuild $out/bin/magebuild";
  };

  guest = config.microvm.vms.preview-1.config.config;

  previewConfig = (pkgs.formats.toml { }).generate "preview.toml" {
    inherit domain;
    state_dir = "/var/lib/preview";
    slots = builtins.length slots;
    pool = "preview";
    template_snapshot = "preview/template@ready";
    slot_root = "${state}/zfs";
    routes_map = "/var/lib/preview/routes.map";
    guest_gateway = "10.0.2.2";
    guest_uid = 1000;
    guest_gid = 1000;
    env_php = {
      template = "app/shared/app/etc/env.php.in";
      targets = [ "app/shared/app/etc/env.php" "app/current/app/etc/env.php" ];
    };
    ports = {
      cid_base = 100;
      http_base = 8100;
      mysql_base = 3300;
      redis_base = 6300;
      search_base = 9300;
    };
    postgres = {
      socket_dir = "/run/preview-postgresql";
      user = "postgres";
      template_database = "tpl";
    };
    taper = {
      binary = "/var/lib/preview/bin/taper";
      mysql_database = "magento";
    };
    build = {
      repo = "git@github.com:cresset-tools/bougie-license-demo.git";
      ssh_key = "/var/lib/preview/deploy_key";
      composer_auth = "/var/lib/preview/auth.json";
      magebuild = "${magebuild}/bin/magebuild";
      guest_php = "${guest.system.build.previewMagentoPhp}/bin/magento-php";
    };
    webhook = {
      listen = "127.0.0.1:8099";
      secret_file = "/var/lib/preview/webhook-secret";
      ignore_branches = [ "main" ];
    };
  };
in
{
  boot.supportedFilesystems = [ "zfs" ];
  boot.kernelModules = [ "vhost_vsock" ];
  boot.zfs.forceImportRoot = false;
  networking.hostId = "4cf25964";

  microvm.vms = lib.listToAttrs (map (n: {
    name = "preview-${toString n}";
    value = {
      config = import ./preview-vm.nix n;
      # A slot runs only while a preview is assigned to it.
      autostart = false;
    };
  }) slots);

  # microvm.nix starts a VM's virtiofsd pair under supervisord, which takes 2 s
  # to call them started and 3 s to stop. Started directly they are ready in
  # milliseconds, and nothing about them needs a graceful stop.

  imports = [ { systemd.services = lib.listToAttrs (map (n:
    let
      vm = "preview-${toString n}";
      guest = config.microvm.vms.${vm}.config.config;
      # The root share's source is a placeholder in the guest's own config
      # (see there); this is where the real one goes in.
      shares = map (s: if s.tag == "root" then s // { source = "${guest.system.build.previewRootfs}"; } else s) guest.microvm.shares;
      sock = share: "${vm}-virtiofs-${share.tag}.sock";
      # Only the slot's contents change under a running VM; the guest may
      # cache the other shares for good.
      virtiofsd = share: ''
        ${pkgs.virtiofsd}/bin/virtiofsd --socket-path=${sock share} --socket-group=kvm \
          --shared-dir=${share.source} --rlimit-nofile 1048576 --thread-pool-size 4 \
          --cache=always --inode-file-handles=prefer &
      '';
    in {
      name = "microvm-virtiofsd@${vm}";
      value = {
        overrideStrategy = "asDropin";
        serviceConfig = {
          ExecStart = [ "" (pkgs.writeShellScript "virtiofsd-${vm}" ''
            rm -f ${lib.concatMapStringsSep " " sock shares}
            ${lib.concatMapStrings virtiofsd shares}
            until ${lib.concatMapStringsSep " && " (s: "[ -S ${sock s} ]") shares}; do sleep 0.005; done
            ${pkgs.systemd}/bin/systemd-notify --ready
            wait -n
          '') ];
          KillSignal = "SIGKILL";
          Restart = lib.mkForce "no";
        };
      };
    }) slots); } ];

  environment.systemPackages = [ preview magebuild ];
  environment.etc."preview/config.toml".source = previewConfig;

  users.groups.preview = { };
  users.users.preview-postgres = { isSystemUser = true; group = "preview"; };
  users.users.preview-taper = { isSystemUser = true; group = "preview"; };

  # One PostgreSQL for every slot, on its own dataset in the pool (so with
  # sync=disabled: a crash can lose the last seconds, never consistency, and
  # nothing in it is precious). It holds the template database and a copy of
  # it per slot; only local sockets, and only the preview group reaches them.
  systemd.services.preview-postgresql = {
    description = "PostgreSQL for preview environments";
    wantedBy = [ "multi-user.target" ];
    after = [ "preview-pool.service" ];
    requires = [ "preview-pool.service" ];
    unitConfig.ConditionPathExists = "/var/lib/preview/postgresql/bin/postgres";
    serviceConfig = {
      User = "preview-postgres";
      Group = "preview";
      RuntimeDirectory = "preview-postgresql";
      RuntimeDirectoryMode = "0750";
      ExecStart = lib.concatStringsSep " " [
        "/var/lib/preview/postgresql/bin/postgres -D ${state}/pg"
        "-c listen_addresses= -c unix_socket_directories=/run/preview-postgresql"
        "-c max_connections=300 -c shared_buffers=512MB -c synchronous_commit=off"
      ];
      KillMode = "mixed";
      KillSignal = "SIGINT";
      TimeoutStopSec = 30;
      Restart = "on-failure";
    };
  };

  # A slot's taper: MySQL, Redis and search for that slot's database, on the
  # slot's ports on the loopback, where the guest reaches them as 10.0.2.2.
  systemd.services."preview-taper@" = {
    description = "taper for preview slot %i";
    after = [ "preview-postgresql.service" ];
    requires = [ "preview-postgresql.service" ];
    serviceConfig = {
      ExecStart = "${preview}/bin/preview taper %i";
      User = "preview-taper";
      Group = "preview";
      RuntimeDirectory = "preview-taper-%i";
      Restart = "on-failure";
      RestartSec = 1;
    };
  };

  # GitHub's push webhook. It runs as root: it clones datasets, starts units
  # and copies databases.
  systemd.services.preview-webhook = {
    description = "Preview environments: GitHub webhook";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "preview-postgresql.service" ];
    wants = [ "network-online.target" ];
    unitConfig.ConditionPathExists = "/var/lib/preview/webhook-secret";
    path = [ config.boot.zfs.package pkgs.systemd pkgs.git pkgs.openssh ];
    serviceConfig = {
      ExecStart = "${preview}/bin/preview serve";
      Restart = "on-failure";
      RestartSec = 5;
    };
  };

  systemd.tmpfiles.rules = [
    "d ${state} 0755 root root -"
    "d /var/lib/preview 0755 root root -"
    "d ${tls} 0750 root nginx -"
    "f /var/lib/preview/routes.map 0644 root root -"
    "d /var/lib/preview/bin 0755 root root -"
  ];

  # Create the pool on first boot, import it on later ones. A file vdev is
  # not found by the normal import scan, hence -d.
  systemd.services.preview-pool = {
    description = "ZFS pool for preview environments";
    wantedBy = [ "multi-user.target" ];
    after = [ "systemd-tmpfiles-setup.service" "systemd-modules-load.service" ];
    unitConfig.RequiresMountsFor = "/srv";
    path = [ config.boot.zfs.package pkgs.coreutils pkgs.kmod ];
    serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
    script = ''
      # After a switch that first adds ZFS, the booted system's module tree has
      # no zfs.ko; the kernel is the same one, so take it from the current system.
      modprobe zfs 2>/dev/null || modprobe -d /run/current-system/kernel-modules zfs
      if ! zpool list preview >/dev/null 2>&1; then
        if [ -e ${state}/pool.img ]; then
          zpool import -d ${state} preview
        else
          # No dataset is mounted under the pool's own name: the VMs' share
          # directories (${state}/zfs/slot-N) exist before the pool does, and
          # ZFS will not mount over a directory that has entries.
          truncate -s 150G ${state}/pool.img
          zpool create -o ashift=12 -O sync=disabled -O compression=lz4 -O atime=off \
            -O xattr=sa -O acltype=posixacl -O mountpoint=none preview ${state}/pool.img
          zfs create -o mountpoint=${state}/template preview/template
        fi
      fi
      zfs list preview/pg >/dev/null 2>&1 || zfs create -o mountpoint=${state}/pg preview/pg
      chown preview-postgres:preview ${state}/pg
      chmod 700 ${state}/pg
    '';
  };

  # The wildcard certificate is placed by hand for the trial (DNS-01, done
  # from a workstation). Until it is there, a self-signed one keeps nginx,
  # and with it the test reports site, starting.
  systemd.services.preview-tls-placeholder = {
    wantedBy = [ "multi-user.target" ];
    before = [ "nginx.service" ];
    requiredBy = [ "nginx.service" ];
    after = [ "systemd-tmpfiles-setup.service" ];
    path = [ pkgs.openssl ];
    serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
    script = ''
      [ -s ${tls}/fullchain.pem ] && exit 0
      openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
        -subj "/CN=*.${domain}" -keyout ${tls}/key.pem -out ${tls}/fullchain.pem
      chgrp nginx ${tls}/key.pem ${tls}/fullchain.pem
      chmod 640 ${tls}/key.pem
    '';
  };

  services.nginx = {
    appendHttpConfig = ''
      map $host $preview_port {
        hostnames;
        default 0;
        include /var/lib/preview/routes.map;
      }
    '';
    # The webhook; nothing else of the daemon is reachable from outside.
    virtualHosts.${domain} = {
      forceSSL = true;
      sslCertificate = "${tls}/fullchain.pem";
      sslCertificateKey = "${tls}/key.pem";
      locations."= /hook".proxyPass = "http://127.0.0.1:8099";
      locations."/".return = "404";
    };
    virtualHosts."*.${domain}" = {
      forceSSL = true;
      sslCertificate = "${tls}/fullchain.pem";
      sslCertificateKey = "${tls}/key.pem";
      locations."/".extraConfig = ''
        if ($preview_port = 0) { return 404 "no such preview\n"; }
        proxy_pass http://127.0.0.1:$preview_port;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_read_timeout 120s;
        proxy_buffer_size 32k;
        proxy_buffers 16 32k;
      '';
    };
  };
}
