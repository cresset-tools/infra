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
# while a preview is assigned. `preview up` (below) clones the template,
# starts the VM and adds the route. The guest boots straight into nginx and
# php-fpm (see ./preview-vm.nix), so a cold start is short enough not to need
# a pool of waiting VMs.
let
  slots = [ 1 2 3 ];
  domain = "preview.cresset.tools";
  state = "/srv/preview";
  tls = "/var/lib/preview/tls";

  # preview up <name> <slot> | down <slot> | exec <slot> [command] | list
  preview = pkgs.writeShellApplication {
    name = "preview";
    runtimeInputs = [ config.boot.zfs.package pkgs.systemd pkgs.coreutils pkgs.gnused pkgs.nginx pkgs.socat ];
    text = ''
      routes=/var/lib/preview/routes.map
      case "''${1:-}" in
        up)
          name=$2; slot=$3
          [[ $name =~ ^[a-z0-9-]{1,40}$ ]] || { echo "bad name" >&2; exit 1; }
          if zfs list "preview/slot-$slot" >/dev/null 2>&1; then "$0" down "$slot"; fi
          zfs clone -o sync=disabled -o "mountpoint=${state}/zfs/slot-$slot/d" "preview/template@ready" "preview/slot-$slot"
          systemctl start "microvm@preview-$slot"
          echo "$name.${domain} 81$(printf %02d "$slot");" >> "$routes"
          systemctl reload nginx
          echo "https://$name.${domain}"
          ;;
        down)
          slot=$2
          # Nothing in a preview is worth a clean shutdown: its dataset is
          # destroyed two lines down. Killing takes milliseconds, a guest
          # shutdown seconds.
          systemctl kill --signal=KILL "microvm@preview-$slot" 2>/dev/null || true
          systemctl stop "microvm@preview-$slot" "microvm-virtiofsd@preview-$slot" || true
          systemctl reset-failed "microvm@preview-$slot" 2>/dev/null || true
          sed -i "/ 81$(printf %02d "$slot");$/d" "$routes"
          systemctl reload nginx || true
          if zfs list "preview/slot-$slot" >/dev/null 2>&1; then zfs destroy "preview/slot-$slot"; fi
          ;;
        exec|root)
          # preview exec <slot> < script   (or: preview exec <slot> 'command')
          # `root` is the same as root instead of magento.
          port=7000; [ "$1" = root ] && port=7001
          slot=$2
          if [ $# -ge 3 ]; then printf '%s\n' "$3"; else cat; fi \
            | socat -t 3600 - "VSOCK-CONNECT:$((100 + slot)):$port"
          ;;
        list)
          cat "$routes"; zfs list -r -o name,used,refer,origin preview
          ;;
        *) echo "usage: preview up <name> <slot> | down <slot> | exec <slot> [command] | list" >&2; exit 1 ;;
      esac
    '';
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

  environment.systemPackages = [ preview ];

  systemd.tmpfiles.rules = [
    "d ${state} 0755 root root -"
    "d /var/lib/preview 0755 root root -"
    "d ${tls} 0750 root nginx -"
    "f /var/lib/preview/routes.map 0644 root root -"
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
      zpool list preview >/dev/null 2>&1 && exit 0
      [ -e ${state}/pool.img ] && zpool import -d ${state} preview && exit 0
      # No dataset is mounted under the pool's own name: the VMs' share
      # directories (${state}/zfs/slot-N) exist before the pool does, and
      # ZFS will not mount over a directory that has entries.
      truncate -s 150G ${state}/pool.img
      zpool create -o ashift=12 -O sync=disabled -O compression=lz4 -O atime=off \
        -O xattr=sa -O acltype=posixacl -O mountpoint=none preview ${state}/pool.img
      zfs create -o mountpoint=${state}/template preview/template
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
