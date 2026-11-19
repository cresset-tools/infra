{ config, pkgs, ... }:

# A deploy target for magebuild's bench shop (products/mage/benchmarks/
# large-shop): a full Mage-OS with every free module that resolves, deployed
# by its CI exactly like the demo store, so deploys of a large shop can be
# measured on real hardware. Nothing serves it: no vhost, no php-fpm, no cron.
#
# Kept apart from the demo store: its own user (so a bench deploy cannot write
# the live tree), its own database (socket auth, no password), OpenSearch
# index prefix `bench`, Redis DBs 4 and 5.
#
# One-time setup after the first switch: unpack a bench artifact and run
# setup:install as magento-bench (see the bench shop's README); that writes
# shared/app/etc/env.php with the crypt key, which every deploy then derives
# its env.php from.
let
  root = "/var/lib/magento-bench";
in
{
  users.users.magento-bench = {
    isSystemUser = true;
    group = "magento-bench";
    description = "magebuild bench shop (deploy target, not served)";
    home = "${root}/home";
    createHome = true;
    shell = pkgs.bashInteractive;
    openssh.authorizedKeys.keys = config.users.users.root.openssh.authorizedKeys.keys ++ [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBwJA/3zp/KP2PZiKmGqz9XUOngZqD2tB9ZcL22OlGfe magebuild-bench CI deploy"
    ];
  };
  users.groups.magento-bench = { };

  systemd.tmpfiles.rules = [
    "d ${root}                0755 magento-bench magento-bench -"
    "d ${root}/releases       0755 magento-bench magento-bench -"
    "d ${root}/shared         0755 magento-bench magento-bench -"
    "d ${root}/shared/app     0755 magento-bench magento-bench -"
    "d ${root}/shared/app/etc 0755 magento-bench magento-bench -"
    "d ${root}/tmp            0755 magento-bench magento-bench -"
  ];

  # Unix-socket auth: magento-bench connects as itself over
  # /run/mysqld/mysqld.sock, so there is no password to keep.
  services.mysql = {
    ensureDatabases = [ "magento_bench" ];
    ensureUsers = [
      {
        name = "magento-bench";
        ensurePermissions."magento_bench.*" = "ALL PRIVILEGES";
      }
    ];
  };
}
