{ pkgs, ... }:

# The guest of ./demo-runner.nix. Everything here is rebuilt from this file on
# a switch except /var, a persistent volume that holds podman's image store,
# the runner's registration and the caches jobs warm up for each other.

let
  repo = "cresset-tools/bougie-license-demo";
  # The official runner image (Ubuntu 24.04), pinned by digest. It already has
  # what deploy.yml calls (curl, git, sha256sum, ssh-agent), and the release
  # binaries jobs download (setup-bougie's bougie, magebuild) are glibc builds,
  # which is why jobs run in Ubuntu and not on this NixOS guest.
  #
  # The runner is registered with --disableupdate, so it never updates itself;
  # GitHub stops sending jobs to a runner some months after its version is
  # superseded, so bump this when a release notes a new minimum.
  image = "ghcr.io/actions/actions-runner:2.338.2@sha256:6d0b2d80d80f143f77dcb676e50920100bbb547e39e883454d57f45ab8c31300";
  state = "/var/lib/demo-runner";
  # What config.sh writes and run.sh signs in with. The runner's own
  # credentials: they let it take this repo's jobs and nothing else.
  registration = ".runner .credentials .credentials_rsaparams";
in
{
  networking.hostName = "demo-runner";
  system.stateVersion = "25.11";

  microvm = {
    hypervisor = "qemu";
    vcpu = 6;
    mem = 8192;
    # qemu's user-mode network: outbound only (GitHub, the Composer repos, ssh
    # to demo.bougie.tools), NAT inside qemu, nothing to configure on the host.
    interfaces = [
      {
        type = "user";
        id = "usernet";
        mac = "02:00:00:d0:00:01";
      }
    ];
    volumes = [
      {
        image = "var.img";
        mountPoint = "/var";
        size = 100 * 1024;
      }
    ];
    # Read by qemu on the host when the VM starts, handed to the guest as a
    # systemd credential. Only used until the runner is registered.
    credentialFiles.registration-token = "/var/lib/demo-runner/registration-token";
  };

  networking.useNetworkd = true;
  systemd.network.networks."10-uplink" = {
    matchConfig.Type = "ether";
    networkConfig.DHCP = "yes";
  };

  # Owned by the image's `runner` user (uid 1001). cache and local are the
  # job's ~/.cache and ~/.local: bougie's home, its PHP interpreters and the
  # Composer dist cache magebuild reads, so a job starts where the last one
  # left off.
  systemd.tmpfiles.rules = [
    "d ${state} 0755 root root -"
    "d ${state}/registration 0700 1001 1001 -"
    "d ${state}/cache 0755 1001 1001 -"
    "d ${state}/local 0755 1001 1001 -"
  ];

  # Register once, with the token from the host, and keep what config.sh
  # wrote. Skipped once that is there: the token has expired by then anyway.
  systemd.services.demo-runner-register = {
    wantedBy = [ "multi-user.target" ];
    before = [ "podman-demo-runner.service" ];
    requiredBy = [ "podman-demo-runner.service" ];
    wants = [ "network-online.target" ];
    after = [ "network-online.target" "systemd-tmpfiles-setup.service" ];
    unitConfig.ConditionPathExists = "!${state}/registration/.runner";
    path = [ pkgs.podman ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ImportCredential = "registration-token";
    };
    script = ''
      set -euo pipefail
      RUNNER_TOKEN=$(cat "$CREDENTIALS_DIRECTORY/registration-token" 2>/dev/null || true)
      if [ -z "$RUNNER_TOKEN" ]; then
        echo "no registration token: put one in /var/lib/demo-runner/registration-token on the host and restart microvm@demo-runner" >&2
        exit 1
      fi
      export RUNNER_TOKEN
      podman run --rm --network=host --env RUNNER_TOKEN \
        --volume ${state}/registration:/registration \
        ${image} \
        bash -c './config.sh --unattended --disableupdate --replace \
          --url https://github.com/${repo} --token "$RUNNER_TOKEN" \
          --name demo-runner --labels demo-runner \
          && cp ${registration} /registration/'
    '';
  };

  virtualisation.podman.enable = true;
  virtualisation.oci-containers = {
    backend = "podman";
    containers.demo-runner = {
      inherit image;
      # The VM is the boundary; a podman bridge inside it only adds NAT (and
      # its DNS did not resolve through qemu's user network).
      extraOptions = [ "--network=host" ];
      volumes = [
        "${state}/registration:/registration:ro"
        "${state}/cache:/home/runner/.cache"
        "${state}/local:/home/runner/.local"
      ];
      cmd = [
        "bash"
        "-c"
        "cd /registration && cp ${registration} /home/runner/ && cd /home/runner && exec ./run.sh"
      ];
    };
  };
}
