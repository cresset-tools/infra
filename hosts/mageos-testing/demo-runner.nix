{ inputs, ... }:

# A GitHub Actions runner for cresset-tools/bougie-license-demo (private), in a
# microvm so the demo's builds never share a kernel with the Mage-OS test
# pipeline or this host's own secrets. The VM itself is ./demo-runner-vm.nix:
# NixOS with podman, running the official runner image.
#
# Why here: the box is a mostly idle Ryzen 5 3600 (the nightly test run is at
# 01:34), 35 ms from demo.bougie.tools. GitHub's runners are ~100 ms away, and
# start every build with cold caches.
#
# Manual step after the first switch, once: register the runner. Get a
# registration token (valid for an hour) from the repo's Settings → Actions →
# Runners → New self-hosted runner, or
#
#   gh api -X POST repos/cresset-tools/bougie-license-demo/actions/runners/registration-token -q .token
#
# and on this host
#
#   install -m600 /dev/stdin /var/lib/demo-runner/registration-token   # paste, ^D
#   systemctl restart microvm@demo-runner
#
# The guest registers with it and keeps the runner's own credentials on its
# /var volume; the token is not used again. Only when that volume is lost does
# the runner need registering again.
{
  imports = [ inputs.microvm.nixosModules.host ];

  systemd.tmpfiles.rules = [
    "d /var/lib/demo-runner 0700 root root -"
    # qemu refuses to start the VM when a credential file is missing.
    "f /var/lib/demo-runner/registration-token 0600 root root -"
  ];

  microvm.vms.demo-runner.config = ./demo-runner-vm.nix;
}
