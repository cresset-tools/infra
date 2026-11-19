{ pkgs, lib, ... }:

# The guest of ./demo-runner.nix. Everything here is rebuilt from this file on
# a switch except /var, a persistent volume that holds podman's image store,
# the runner's registration and the caches jobs warm up for each other.

let
  # The official runner image (Ubuntu 24.04), pinned by digest. It already has
  # what deploy.yml calls (curl, git, sha256sum, ssh-agent), and the release
  # binaries jobs download (setup-bougie's bougie, magebuild) are glibc builds,
  # which is why jobs run in Ubuntu and not on this NixOS guest.
  #
  # The runner is registered with --disableupdate, so it never updates itself;
  # GitHub stops sending jobs to a runner some months after its version is
  # superseded, so bump this when a release notes a new minimum.
  image = "ghcr.io/actions/actions-runner:2.338.2@sha256:6d0b2d80d80f143f77dcb676e50920100bbb547e39e883454d57f45ab8c31300";
  # What config.sh writes and run.sh signs in with. The runner's own
  # credentials: they let it take this repo's jobs and nothing else.
  registration = ".runner .credentials .credentials_rsaparams";

  # One runner container per repo, each with its own registration and caches,
  # so two jobs never write the same bougie home. A repo-level runner serves
  # one repository only.
  runners = {
    demo-runner = {
      repo = "cresset-tools/bougie-license-demo";
      credential = "registration-token";
      tokenFile = "/var/lib/demo-runner/registration-token";
    };
    # The magebuild bench shop (products/mage/benchmarks/large-shop).
    bench-runner = {
      repo = "cresset-tools/magebuild-bench";
      credential = "registration-token-bench";
      tokenFile = "/var/lib/demo-runner/registration-token-bench";
    };
  };

  # Owned by the image's `runner` user (uid 1001). cache and local are the
  # job's ~/.cache and ~/.local: bougie's home, its PHP interpreters and the
  # Composer dist cache magebuild reads, so a job starts where the last one
  # left off.
  dirs = name: let state = "/var/lib/${name}"; in [
    "d ${state} 0755 root root -"
    "d ${state}/registration 0700 1001 1001 -"
    "d ${state}/cache 0755 1001 1001 -"
    "d ${state}/local 0755 1001 1001 -"
  ];

  # Register once, with the token from the host, and keep what config.sh
  # wrote. Skipped once that is there: the token has expired by then anyway.
  register = name: r: let state = "/var/lib/${name}"; in {
    wantedBy = [ "multi-user.target" ];
    before = [ "podman-${name}.service" ];
    requiredBy = [ "podman-${name}.service" ];
    wants = [ "network-online.target" ];
    after = [ "network-online.target" "systemd-tmpfiles-setup.service" ];
    unitConfig.ConditionPathExists = "!${state}/registration/.runner";
    path = [ pkgs.podman ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ImportCredential = r.credential;
    };
    script = ''
      set -euo pipefail
      RUNNER_TOKEN=$(cat "$CREDENTIALS_DIRECTORY/${r.credential}" 2>/dev/null || true)
      if [ -z "$RUNNER_TOKEN" ]; then
        echo "no registration token: put one in ${r.tokenFile} on the host and restart microvm@demo-runner" >&2
        exit 1
      fi
      export RUNNER_TOKEN
      podman run --rm --network=host --env RUNNER_TOKEN \
        --volume ${state}/registration:/registration \
        ${image} \
        bash -c './config.sh --unattended --disableupdate --replace \
          --url https://github.com/${r.repo} --token "$RUNNER_TOKEN" \
          --name ${name} --labels ${name} \
          && cp ${registration} /registration/'
    '';
  };

  container = name: let state = "/var/lib/${name}"; in {
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
    # Read by qemu on the host when the VM starts, handed to the guest as
    # systemd credentials. Only used until each runner is registered.
    credentialFiles = lib.mapAttrs' (_: r: lib.nameValuePair r.credential r.tokenFile) runners;
  };

  networking.useNetworkd = true;
  systemd.network.networks."10-uplink" = {
    matchConfig.Type = "ether";
    networkConfig.DHCP = "yes";
  };

  systemd.tmpfiles.rules = lib.concatLists (lib.mapAttrsToList (name: _: dirs name) runners);

  systemd.services = lib.mapAttrs' (name: r: lib.nameValuePair "${name}-register" (register name r)) runners;

  virtualisation.podman.enable = true;
  virtualisation.oci-containers = {
    backend = "podman";
    containers = lib.mapAttrs (name: _: container name) runners;
  };
}
