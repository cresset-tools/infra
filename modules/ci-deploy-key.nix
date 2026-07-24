# Root SSH authorization for the CD workflow (.github/workflows/deploy.yml).
#
# On merge to main, deploy.yml builds each host's closure on the GitHub runner
# and uses deploy-rs to copy + activate it over SSH as root. This module adds
# that workflow's public key to the importing host's root authorized_keys. The
# matching credential is stored only as the infra repo's DEPLOY_SSH_KEY Actions
# secret; rotate by regenerating the pair and redeploying every importing host.
#
# Imported only by the hosts CD targets (the persistent x86_64 production
# boxes). demo/origin/mageos-testing deploy by hand and don't import this.
{ ... }:
{
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAueuxjHoxb4+vOFu9nsWFq2vopX/mtienN+XJKbUuou infra CD (deploy-rs)"
  ];
}
