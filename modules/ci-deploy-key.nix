# Root SSH authorization for the CD workflow (.github/workflows/deploy.yml).
#
# On merge to main, deploy.yml builds each host's closure on the GitHub runner
# and uses deploy-rs to copy + activate it over SSH as root. This module adds
# that workflow's public key to the importing host's root authorized_keys.
#
# The keypair is generated (and rotated) by .github/workflows/provision-cd-key.yml
# INSIDE a GitHub Actions runner: the private half goes straight into the
# `production` environment's DEPLOY_SSH_KEY secret and never leaves GitHub; that
# run prints the public half to paste below. No operator machine ever holds the
# private key.
#
# Imported only by the hosts CD targets (the persistent x86_64 production
# boxes). demo/origin/mageos-testing deploy by hand and don't import this.
{ ... }:
{
  users.users.root.openssh.authorizedKeys.keys = [
    # Provisioned by .github/workflows/provision-cd-key.yml (2026-07-24). The
    # private half lives only in the production/DEPLOY_SSH_KEY env secret.
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKxESQhIZRhl6NEucrPX6YJfnQdppVEwLDsj95X3Ti0i infra CD (deploy-rs)"
  ];
}
