# sops-nix secrets for the licensing demo host.
#
# Decrypted at activation with the box's own SSH host key
# (/etc/ssh/ssh_host_ed25519_key), which is planted during provisioning via
# `nixos-anywhere --extra-files` (pre-generated, kept out of git under
# ~/.config/sops/demo-host/). Admins edit via the age recipients in ../.sops.yaml.
#
# Import alongside sops-nix's own module, e.g. in hosts/demo/configuration.nix:
#   imports = [ inputs.sops-nix.nixosModules.sops ../../modules/secrets.nix ];
{ ... }:
{
  sops.defaultSopsFile = ../secrets/demo.yaml;
  sops.age.sshKeyPaths = [ "/etc/ssh/ssh_host_ed25519_key" ];

  # Each is decrypted to /run/secrets/<name> (root-only by default). The demo host
  # config renders these into the container env / Magento env.php at activation.
  #
  # postgres/sconce_password is also read directly by the demo-postgres-password
  # oneshot, which runs as the `postgres` user (peer auth) — so it must own the
  # file, otherwise `cat` hits Permission denied on the default root:0400. The
  # mariadb password stays root-owned: its oneshot runs as root.
  sops.secrets = {
    "sconce/secret_key" = { };
    "mollie/apikey_test" = { };
    "postgres/sconce_password" = { owner = "postgres"; };
    "mariadb/magento_password" = { };
    "magento/crypt_key" = { };
  };
}
