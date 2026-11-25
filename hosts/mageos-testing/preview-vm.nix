# One preview environment: nginx and PHP-FPM for a single branch of a Magento
# shop, in a microvm. The guest holds no state of its own and runs no database:
# /data is a virtiofs share of a ZFS clone the host makes per preview
# (./preview.nix) and holds the deployed releases, and MySQL, Redis, AMQP and
# search are one taper per preview on the host, reached at 10.0.2.2 (the
# host's loopback as user-mode networking shows it to the guest).
#
# There is no sshd. The host writes releases straight into the dataset, and
# runs commands in the guest through an agent on a vsock port (the way incus
# does it): a socket-activated unit that runs what it is sent as the `magento`
# user. `preview exec <slot>` on the host is the client. That saves host key
# generation at every boot and leaves the guest with one listening TCP port.
#
# The guest is identical for every slot except its forwarded port and vsock
# id, so starting a preview is "clone a dataset, start a VM".
slot:
{ config, pkgs, lib, ... }:

let
  php = pkgs.php84.buildEnv {
    extensions = { enabled, all }: enabled ++ (with all; [
      bcmath calendar exif ftp gd gettext gmp intl opcache pcntl
      pdo_mysql redis shmop soap sockets sysvmsg sysvsem sysvshm xsl zip
    ]);
    extraConfig = ''
      memory_limit = 2G
      max_execution_time = 1800
      opcache.memory_consumption = 256
      opcache.max_accelerated_files = 60000
      realpath_cache_size = 10M
    '';
  };
  magentoPhp = pkgs.writeShellScriptBin "magento-php" ''exec ${php}/bin/php "$@"'';

  # ---- Direct boot ------------------------------------------------------
  # A NixOS guest takes about ten seconds to reach nginx: 2.8 s in the
  # systemd initrd, 1.6 s switching root, the rest starting units nobody here
  # needs. So this guest does not boot NixOS, and has no initrd either.
  #
  # The kernel is our own: `make tinyconfig` plus ./preview-kernel.fragment,
  # which builds in the six drivers the guest uses (virtio-pci, virtiofs,
  # virtio-net, vsock, balloon, rng) and nothing loadable. With virtiofs in
  # the kernel it mounts its root itself: a third, read-only share holding
  # busybox, /sbin/init and the mount points (`rootfs` below). /sbin/init
  # mounts the other two shares, brings the network up and execs systemd.
  #
  # systemd runs as PID 1 with a unit directory of our own (`units` below):
  # the unit files NixOS generated for nginx, php-fpm and the agent, exactly
  # as written, plus journald and the targets they hang off. NixOS's own boot
  # (stage 2, activation, its hundred other units) is what is left out.
  #
  # To change the kernel config: edit the fragment, then in a kernel tree of
  # this version run `make tinyconfig`, `scripts/kconfig/merge_config.sh -m
  # .config preview-kernel.fragment`, `make olddefconfig`, and commit the
  # resulting .config as ./preview-kernel.config.
  kernel = pkgs.linuxManualConfig {
    inherit (pkgs.linux_6_18) src version modDirVersion;
    configfile = ./preview-kernel.config;
  };
  busybox = pkgs.pkgsStatic.busybox;
  etc = pkgs.runCommand "preview-etc" { } ''
    mkdir -p $out
    cat > $out/passwd <<'EOF'
    root:x:0:0:root:/root:/bin/sh
    magento:x:1000:1000:magento:/data/app:/bin/sh
    nginx:x:60:60:nginx:/var/empty:/bin/sh
    nobody:x:65534:65534:nobody:/:/bin/sh
    EOF
    cat > $out/group <<'EOF'
    root:x:0:
    magento:x:1000:
    nginx:x:60:
    nobody:x:65534:
    nogroup:x:65534:
    EOF
    sed -i 's/^ *//' $out/passwd $out/group
    echo "127.0.0.1 localhost preview-${toString slot}" > $out/hosts
    echo "nameserver 10.0.2.3" > $out/resolv.conf
    ln -s ${pkgs.cacert}/etc/ssl $out/ssl
  '';
  agent = pkgs.writeShellScript "preview-agent" ''
    export PATH=${lib.makeBinPath [ pkgs.bash pkgs.coreutils php magentoPhp pkgs.gnugrep pkgs.gnused pkgs.findutils pkgs.procps pkgs.git pkgs.unzip pkgs.mariadb.client pkgs.util-linux config.systemd.package ]}:/bin
    export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
    bash -s
    echo "exit=$?"
  '';
  # /etc/systemd/system for the guest: the unit files NixOS generated for the
  # services defined below, the handful of upstream targets they hang off,
  # and journald. Nothing else exists, so nothing else starts.
  unit = name: "${config.systemd.units.${name}.unit}/${name}";
  upstream = "${config.systemd.package}/example/systemd/system";
  units = pkgs.runCommand "preview-units" { } ''
    mkdir -p $out/preview.target.wants $out/sockets.target.wants $out/sysinit.target.wants
    for u in basic.target sysinit.target sockets.target slices.target paths.target timers.target \
             local-fs.target local-fs-pre.target shutdown.target umount.target final.target \
             systemd-journald.service systemd-journald.socket systemd-journald-dev-log.socket; do
      ln -s ${upstream}/$u $out/$u
    done
    ln -s ${unit "nginx.service"} ${unit "phpfpm-magento.service"} $out/
    ln -s ${unit "preview-agent.socket"} ${unit "preview-agent@.service"} $out/
    ln -s ${unit "preview-agent-root.socket"} ${unit "preview-agent-root@.service"} $out/
    cat > $out/preview.target <<'EOF'
    [Unit]
    Description=Preview environment
    Requires=basic.target
    After=basic.target
    AllowIsolate=yes
    EOF
    sed -i 's/^ *//' $out/preview.target
    ln -s ../nginx.service ../phpfpm-magento.service $out/preview.target.wants/
    ln -s ../preview-agent.socket ../preview-agent-root.socket $out/sockets.target.wants/
    ln -s ../systemd-journald.socket ../systemd-journald-dev-log.socket $out/sockets.target.wants/
    ln -s ../systemd-journald.service $out/sysinit.target.wants/
  '';
  init = pkgs.writeScript "preview-init" ''
    #!/bin/sh
    export PATH=/bin
    mount -t proc proc /proc
    mount -t sysfs sys /sys
    mount -t tmpfs -o mode=1777 tmp /tmp
    mount -t tmpfs run /run
    mount -t tmpfs var /var
    mount -t tmpfs root /root
    mkdir -p /dev/shm
    mount -t tmpfs -o mode=1777 shm /dev/shm
    mount -t virtiofs -o ro ro-store /nix/store
    mount -t virtiofs slot /slot
    hostname preview-${toString slot}
    ip link set lo up
    ip addr add 10.0.2.15/24 dev eth0
    ip link set eth0 up
    ip route add default via 10.0.2.2
    # qemu's user network forwards the host's connections to this address
    # only once it has seen a packet from it; until then they are dropped
    # and the host's TCP retries cost 7 s. So say hello first.
    # In the background: busybox's arping sends at once but then sits out its
    # whole one-second deadline.
    arping -c 1 -w 1 -I eth0 10.0.2.2 >/dev/null 2>&1 &
    mkdir -p /var/tmp /var/log /var/cache /var/lib
    export TERM=dumb
    exec ${config.systemd.package}/lib/systemd/systemd
  '';
  # The root filesystem: read-only, the same for every slot. Everything in
  # it is a real file, because /nix/store is not there yet when it is used.
  rootfs = pkgs.runCommand "preview-rootfs" { } ''
    mkdir -p $out/bin $out/sbin $out/usr/bin $out/etc $out/proc $out/sys $out/dev \
      $out/tmp $out/run $out/var $out/root $out/slot $out/nix/store
    cp ${busybox}/bin/busybox $out/bin/busybox
    for a in $(${busybox}/bin/busybox --list); do [ -e $out/bin/$a ] || ln -s busybox $out/bin/$a; done
    ln -s ../../bin/busybox $out/usr/bin/env
    cp ${init} $out/sbin/init
    cp -rP ${etc}/. $out/etc/
    mkdir -p $out/etc/systemd
    ln -s ${units} $out/etc/systemd/system
    printf '[Journal]\nStorage=volatile\nRuntimeMaxUse=16M\nForwardToConsole=yes\nMaxLevelConsole=notice\n' > $out/etc/systemd/journald.conf
    printf '[Manager]\nShowStatus=no\nDefaultTimeoutStopSec=2s\n' > $out/etc/systemd/system.conf
    : > $out/etc/machine-id
    printf 'ID=preview\nNAME="Preview guest"\n' > $out/etc/os-release
    ln -s /slot/d $out/data
  '';
in
{
  system.build.previewRootfs = rootfs;

  networking.hostName = "preview-${toString slot}";
  system.stateVersion = "25.11";

  microvm = {
    hypervisor = "qemu";
    inherit kernel;
    # The runner always passes -initrd; this kernel has no initrd support and
    # ignores it.
    initrdPath = "${pkgs.writeTextDir "initrd" "none"}/initrd";
    kernelParams = lib.mkForce [
      "quiet"
      "rootfstype=virtiofs"
      "root=root"
      "ro"
      "systemd.unit=preview.target"
      # A dumb console of a known size: otherwise systemd asks the serial
      # port what it can do and how big it is, and waits a second in total
      # for answers nobody sends.
      "systemd.tty.term.console=dumb"
      "systemd.tty.rows.console=24"
      "systemd.tty.columns.console=80"
    ];
    # microvm.nix's defaults plus rtc=on: without a clock device the kernel
    # spends 1.2 s finding out that there is none.
    qemu.machineOpts = {
      accel = "kvm:tcg";
      mem-merge = "on";
      acpi = "on";
      pit = "off";
      pic = "off";
      pcie = "on";
      usb = "off";
      rtc = "on";
    };
    vcpu = 2;
    # Not 2048: qemu's microvm machine hangs in early boot with exactly 2 GiB.
    mem = 1536;
    balloon = true;
    shares = [
      # The root filesystem. (The mount point is for NixOS's benefit only;
      # the kernel mounts this share as / itself.)
      {
        tag = "root";
# What is shared is system.build.previewRootfs; ./preview.nix puts it
        # here when it starts virtiofsd. It cannot be named in this option:
        # the root filesystem contains this guest's unit files, which depend
        # on its mounts, which depend on this list.
        source = "/var/empty";
        mountPoint = "/rootshare";
        proto = "virtiofs";
      }
      {
        tag = "ro-store";
        source = "/nix/store";
        mountPoint = "/nix/.ro-store";
        proto = "virtiofs";
      }
      # The share is a plain directory that exists before any preview does;
      # a preview's ZFS clone is mounted at `d` inside it while the VM is
      # already running, and shows up in the guest as a submount. That is
      # what lets a slot boot ahead of time and wait.
      {
        tag = "slot";
        source = "/srv/preview/zfs/slot-${toString slot}";
        mountPoint = "/slot";
        proto = "virtiofs";
      }
    ];
    vsock.cid = 100 + slot;
    # User-mode networking with a forwarded port: no bridge, no NAT rules and
    # nothing on the host's firewall. The host's nginx reaches the shop on
    # 127.0.0.1:81<slot>.
    interfaces = [
      {
        type = "user";
        id = "usernet";
        mac = "02:00:00:d1:00:0${toString slot}";
      }
    ];
    forwardPorts = [
      { from = "host"; host.address = "127.0.0.1"; host.port = 8100 + slot; guest.port = 80; }
    ];
  };

  # qemu's user network always hands out the same lease, so it is written
  # down here instead of asked for: no DHCP round trip in the boot.
  networking.useNetworkd = true;
  networking.useDHCP = false;
  systemd.network.networks."10-uplink" = {
    matchConfig.Type = "ether";
    address = [ "10.0.2.15/24" ];
    gateway = [ "10.0.2.2" ];
    dns = [ "10.0.2.3" ];
    linkConfig.RequiredForOnline = "no";
  };
  services.resolved.enable = false;
  networking.nameservers = [ "10.0.2.3" ];
  # Only the forwarded port is reachable from outside the guest at all.
  networking.firewall.enable = false;

  # Boot time is the product here. The serial console stays for debugging but
  # prints nothing in a normal boot: at 115200 baud the status lines alone
  # cost seconds.
  boot.consoleLogLevel = 3;
  console.enable = false;
  documentation.enable = false;
  systemd.services.systemd-journal-catalog-update.enable = false;
  systemd.services.systemd-update-done.enable = false;
  services.timesyncd.enable = false;
  security.sudo.enable = false;
  services.nscd.enable = false;
  system.nssModules = lib.mkForce [ ];
  services.logrotate.enable = false;
  systemd.oomd.enable = false;
  systemd.services.lastlog2-import.enable = lib.mkForce false;

  users.groups.magento.gid = 1000;
  users.users.magento = {
    isNormalUser = true;
    uid = 1000;
    group = "magento";
    home = "/data/app";
    createHome = false;
  };

  # The agent: whatever arrives on the socket is a shell script, run as
  # magento in the current release, output and exit status sent back.
  systemd.sockets.preview-agent = {
    wantedBy = [ "sockets.target" ];
    socketConfig = {
      ListenStream = "vsock::7000";
      Accept = true;
    };
  };
  # The same on 7001 as root, for the host's own use (restarting php-fpm,
  # reading the journal).
  systemd.sockets.preview-agent-root = {
    wantedBy = [ "sockets.target" ];
    socketConfig = {
      ListenStream = "vsock::7001";
      Accept = true;
    };
  };
  systemd.services."preview-agent-root@" = {
    serviceConfig = {
      StandardInput = "socket";
      StandardOutput = "socket";
      StandardError = "socket";
      ExecStart = agent;
    };
  };
  systemd.services."preview-agent@" = {
    serviceConfig = {
      User = "magento";
      StandardInput = "socket";
      StandardOutput = "socket";
      StandardError = "socket";
      ExecStart = agent;
    };
  };
  systemd.tmpfiles.rules = [ "L+ /data - - - - /slot/d" ];

  services.phpfpm.pools.magento = {
    user = "magento";
    group = "magento";
    phpPackage = php;
    settings = {
      # nginx's workers run as nobody under the direct boot.
      "listen.mode" = "0666";
      pm = "static";
      "pm.max_children" = 6;
      "catch_workers_output" = true;
    };
  };

  services.nginx = {
    enable = true;
    recommendedGzipSettings = true;
    virtualHosts.default = {
      default = true;
      root = "/data/app/current/pub";
      extraConfig = ''
        index index.php;
        client_max_body_size 32m;
      '';
      locations."/".tryFiles = "$uri $uri/ /index.php$is_args$args";
      locations."/static/".extraConfig = ''
        expires max;
        location ~ ^/static/version\d*/ { rewrite ^/static/version\d*/(.*)$ /static/$1 last; }
        try_files $uri =404;
      '';
      locations."/media/".tryFiles = "$uri $uri/ /get.php$is_args$args";
      locations."~ ^/(index|get|static|health_check)\\.php$".extraConfig = ''
        fastcgi_pass unix:/run/phpfpm/magento.sock;
        include ${pkgs.nginx}/conf/fastcgi.conf;
        fastcgi_param HTTPS on;
        fastcgi_buffers 16 16k;
        fastcgi_buffer_size 32k;
        fastcgi_read_timeout 120s;
      '';
    };
  };
  systemd.services.nginx.unitConfig.RequiresMountsFor = "/slot";

  environment.systemPackages = [ php magentoPhp pkgs.mariadb.client pkgs.git pkgs.unzip ];
  users.users.root.password = "";
  users.users.nginx.extraGroups = [ "magento" ];
}
