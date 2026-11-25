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
  sops.secrets = {
    "mollie/apikey_test" = { };
    "mariadb/magento_password" = { };
    "magento/crypt_key" = { };
    # The Magento module's management-API service token (re-injected into store
    # config under the box crypt key by bin/seed-and-inject).
    "sconce/service_token" = { };
  };
}
