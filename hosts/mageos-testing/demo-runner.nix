{ inputs, ... }:

# GitHub Actions runners for cresset-tools/bougie-license-demo and
# cresset-tools/magebuild-bench (both private), one container each, in a
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
#   cat > /var/lib/demo-runner/registration-token   # paste, ^D
#   systemctl restart microvm@demo-runner
#
# The guest registers with it and keeps the runner's own credentials on its
# /var volume; the token is not used again. Only when that volume is lost does
# the runner need registering again.
{
  imports = [ inputs.microvm.nixosModules.host ];

  # qemu runs as microvm:kvm and reads the token itself, and refuses to start
  # the VM when a credential file is missing or unreadable.
  systemd.tmpfiles.rules = [
    "d /var/lib/demo-runner 0700 microvm kvm -"
    "f /var/lib/demo-runner/registration-token 0600 microvm kvm -"
    "z /var/lib/demo-runner/registration-token 0600 microvm kvm -"
    # The bench shop's runner (cresset-tools/magebuild-bench), registered the
    # same way with its own token file.
    "f /var/lib/demo-runner/registration-token-bench 0600 microvm kvm -"
    "z /var/lib/demo-runner/registration-token-bench 0600 microvm kvm -"
  ];

  microvm.vms.demo-runner.config = ./demo-runner-vm.nix;
}
