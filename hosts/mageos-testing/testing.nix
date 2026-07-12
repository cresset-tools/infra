{ config, pkgs, lib, ... }:

# Mage-OS integration-testing worker: runs the full mageos-magento2 suite
# against every new master commit (parallel harness from
# cresset-tools/mageos-magento2 branch `mageos-testing`), diagnoses
# failures through the retry ladder, publishes static HTML reports, and
# keeps a git history of per-test baselines.
#
# Manual provisioning steps after first deploy:
#   1. journalctl -u mageos-testing-setup — copy the printed deploy key,
#      add it to cresset-tools/mageos-magento2 as a write deploy key.
#   2. Point mageos-tests.bougie.tools at this host (Cloudflare, DNS-only).
#   3. Place /root/borg/ssh + /root/borg/passphrase for the backup job.

let
  user = "mageos-testing";
  stateDir = "/srv/mageos-testing";
  domain = "mageos-tests.cresset.tools";
  workers = 10;

  # bougie's musl release build is fully static and runs on NixOS as-is
  # (same pattern as hosts/demo).
  bougie = pkgs.stdenv.mkDerivation rec {
    pname = "bougie";
    version = "0.48.0";
    src = pkgs.fetchurl {
      url = "https://github.com/cresset-tools/bougie/releases/download/bougie-v${version}/bougie-x86_64-unknown-linux-musl.tar.gz";
      hash = "sha256:0p02vx0nxpfm2cjkixmfjhq1hfp6lsz9b2i9j97nv9d90wvk43qz";
    };
    sourceRoot = ".";
    dontBuild = true;
    installPhase = ''
      install -Dm755 $(find . -name bougie -type f | head -1) $out/bin/bougie
    '';
  };

  runnerPath = with pkgs; [
    bougie
    bash
    coreutils
    curl
    gawk
    git
    gnugrep
    gnused
    gnutar
    gzip
    openssh
    procps
    util-linux
  ];

  runner = pkgs.writeShellScript "mageos-testing-run" ''
    # Test the newest mageos-magento2 main commit if it hasn't been tested
    # yet. Safe under a timer: flock guards concurrency, unchanged HEAD
    # exits immediately.
    set -euo pipefail

    DIR=${stateDir}/mageos-master
    STATE=${stateDir}
    WORKERS=${toString workers}
    export GIT_SSH_COMMAND="ssh -i $HOME/.ssh/fork-deploy -o IdentitiesOnly=yes"

    mkdir -p "$STATE/logs"
    exec 9>"$STATE/run.lock"
    flock -n 9 || { echo "another run is in progress"; exit 0; }

    cd "$DIR"
    git fetch mageos main --no-tags -q
    NEW=$(git rev-parse mageos/main)
    LAST=$(cat "$STATE/last-run-sha" 2>/dev/null || echo "")
    if [ "$NEW" = "$LAST" ]; then
        echo "no new commit on mageos/main ($NEW)"
        exit 0
    fi

    LOG=$STATE/logs/$NEW.log
    exec > >(tee "$LOG") 2>&1
    echo "=== testing mageos/main $NEW (previous: ''${LAST:-none}) $(date -Is) ==="

    # The worktree is disposable — the branch is the source of truth.
    # bougie sync/pin normalize tracked files (composer.json formatting),
    # and a dirty tree blocks the rebase on the next run.
    git reset --hard -q

    # Carry the harness branch onto the new upstream commit. A conflict
    # means upstream touched a patched file — stop loudly rather than test
    # a half-patched tree.
    git rebase --quiet "$NEW" mageos-testing || {
        git rebase --abort
        echo "FATAL: harness branch no longer rebases onto mageos/main — manual fixup needed"
        exit 2
    }
    # Keep the fork's copy current (rebase rewrites history). Non-fatal:
    # a push failure must not stop the test run.
    git push origin mageos-testing --force-with-lease -q \
        || echo "warning: could not push mageos-testing to fork"

    # (php 8.4 pin is committed in the branch's composer.json —
    # config.platform.php — so no `bougie php pin` here: it rewrites
    # composer.json formatting and dirties the tree.)
    bougie sync
    # Services may be down after an unattended reboot.
    bougie service up

    # Full re-template when anything schema- or dependency-shaped changed;
    # otherwise the per-job resets keep reusing the existing template.
    NEED_TEMPLATE=0
    if [ -z "$LAST" ] || ! git merge-base --is-ancestor "$LAST" "$NEW" 2>/dev/null; then
        NEED_TEMPLATE=1
    elif git diff --name-only "$LAST".."$NEW" \
        | grep -qE 'composer\.lock|db_schema|/Setup/|_files/Magento/|queue_topology|communication\.xml'; then
        NEED_TEMPLATE=1
    fi

    cd "$DIR/dev/tests/integration"
    if [ "$NEED_TEMPLATE" = 1 ]; then
        echo "--- re-templating (schema/deps changed or first run) ---"
        rm -rf tmp/sandbox-* "$DIR/generated/code" "$DIR/generated/metadata"
        SOCK=$HOME/.local/share/bougie/state/services/mariadb/run/mariadb.sock
        CLIENT=$(ls "$HOME"/.local/share/bougie/store/mariadb-*/bin/mariadb | tail -1)
        TENANT=$(cd "$DIR" && bougie run -- sh -c 'echo $BOUGIE_SERVICE_MARIADB_DATABASE')
        {
            echo "DROP DATABASE IF EXISTS \`$TENANT\`; CREATE DATABASE \`$TENANT\` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci; GRANT ALL PRIVILEGES ON \`$TENANT\`.* TO '$TENANT'@'localhost';"
            for t in $(seq 1 "$WORKERS"); do echo "DROP DATABASE IF EXISTS \`''${TENANT}_t$t\`;"; done
        } | "$CLIENT" --socket="$SOCK"
        bougie run -- timeout 2700 php ../../../vendor/bin/phpunit -c phpunit-parallel.xml \
            testsuite/Magento/Directory/Model/CurrencyConfigTest.php >/dev/null 2>&1 || true
        TEMPLATE=$(ls -d tmp/sandbox-0-* | head -1)
        [ -f "$TEMPLATE/etc/env.php" ] || { echo "FATAL: template install failed"; exit 2; }
        bougie run -- php -r '$f="'"$TEMPLATE"'/etc/env.php"; $e=include $f; $e["downloadable_domains"]=["localhost","example.com","www.example.com"]; file_put_contents($f,"<?php\nreturn ".var_export($e,true).";\n");'
        bougie run -- sh -c 'rabbitmq-plugins enable rabbitmq_management' || true
        bougie run -- php bin/parallel-prime "$WORKERS"
    fi

    echo "--- running suite (-p $WORKERS) ---"
    rm -f tmp/parallel-logs/*.junit.xml tmp/parallel-logs/diagnosis.json
    bougie run -- php bin/parallel-modules -p "$WORKERS" || true

    echo "--- diagnosing failures ---"
    bougie run -- php bin/diagnose-failures --slots 1,2 || true

    echo "--- generating report ---"
    bougie run -- php bin/generate-report --sha "$NEW" \
        --out "$STATE/reports" --baselines "$STATE/baseline"

    cd "$STATE/baseline"
    git add -A
    git -c user.name=mageos-testing -c user.email=testing@bougie.tools \
        commit -q -m "run $NEW" || true

    echo "$NEW" > "$STATE/last-run-sha"
    echo "=== done $(date -Is) ==="
  '';
in
{
  # bougie's downloaded toolchains (PHP from php-build-standalone, service
  # runtimes) are dynamically linked against glibc but bundle every other
  # library — the loader that nix-ld provides is all they need (verified:
  # `ld-linux-x86-64.so.2 <bougie php> -v` runs with zero missing libs).
  programs.nix-ld.enable = true;

  # bougied spawns service processes with a hardcoded PATH=/usr/bin:/bin
  # (crates/bougie-daemon .. provisioners/rabbitmq.rs), and the runtimes'
  # shell launchers need dirname/sed/grep there. Bind-mount a real FHS
  # tool set onto /usr/bin — envfs was tried first but only resolves
  # exec-style access, not the stat-based lookups shells do.
  fileSystems."/usr/bin" = {
    device = "${pkgs.buildEnv {
      name = "fhs-usr-bin";
      paths = with pkgs; [
        coreutils
        findutils
        gawk
        gnugrep
        gnused
        gnutar
        gzip
        procps
        util-linux
        which
      ];
      pathsToLink = [ "/bin" ];
    }}/bin";
    fsType = "none";
    options = [ "bind" "nofail" ];
  };

  programs.ssh.knownHosts.github = {
    hostNames = [ "github.com" ];
    publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
  };

  users.users.${user} = {
    isSystemUser = true;
    group = user;
    home = stateDir;
    shell = pkgs.bashInteractive;
  };
  users.groups.${user} = { };

  systemd.tmpfiles.rules = [
    # 0755 so nginx can traverse into reports/
    "d ${stateDir} 0755 ${user} ${user} -"
    "d ${stateDir}/reports 0755 ${user} ${user} -"
    "d ${stateDir}/logs 0750 ${user} ${user} -"
    # ProtectSystem=strict in the borg unit needs the stage dir pre-created
    "d /var/lib/borg-stage 0700 root root -"
  ];

  environment.systemPackages = [ bougie pkgs.git ];

  # One-time materialization: deploy key, repo clone, baseline repo.
  # Fetches happen anonymously over https; only the fork push needs the
  # deploy key, so the clone works before the key is registered.
  systemd.services.mageos-testing-setup = {
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "srv.mount" ];
    wants = [ "network-online.target" ];
    path = runnerPath;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      User = user;
      Environment = [ "HOME=${stateDir}" ];
    };
    script = ''
      set -euo pipefail
      mkdir -p $HOME/.ssh $HOME/baseline
      if [ ! -f $HOME/.ssh/fork-deploy ]; then
          ssh-keygen -t ed25519 -N "" -C "mageos-testing@bougie.tools" -f $HOME/.ssh/fork-deploy
          echo "=== REGISTER THIS WRITE DEPLOY KEY ON cresset-tools/mageos-magento2 ==="
          cat $HOME/.ssh/fork-deploy.pub
          echo "======================================================================"
      fi
      if [ ! -d $HOME/mageos-master/.git ]; then
          git clone -b mageos-testing https://github.com/cresset-tools/mageos-magento2.git $HOME/mageos-master
          cd $HOME/mageos-master
          git remote add mageos https://github.com/mage-os/mageos-magento2.git
          git remote set-url --push origin git@github.com:cresset-tools/mageos-magento2.git
      fi
      if [ ! -d $HOME/baseline/.git ]; then
          git -C $HOME/baseline init -q
      fi
    '';
  };

  systemd.services.mageos-testing-run = {
    after = [ "mageos-testing-setup.service" ];
    requires = [ "mageos-testing-setup.service" ];
    path = runnerPath;
    serviceConfig = {
      Type = "oneshot";
      User = user;
      Environment = [ "HOME=${stateDir}" ];
      ExecStart = runner;
      # First run installs the template and primes workers before the
      # ~2.5h suite; leave generous headroom.
      TimeoutStartSec = "8h";
      Nice = 10;
      UMask = "0022";
    };
  };

  systemd.timers.mageos-testing-run = {
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "*-*-* 01:30:00 UTC";
      RandomizedDelaySec = "10m";
      Persistent = true;
    };
  };

  security.acme = {
    acceptTerms = true;
    defaults.email = "jelle@pingiun.com";
  };

  services.nginx = {
    enable = true;
    recommendedOptimisation = true;
    recommendedTlsSettings = true;
    recommendedGzipSettings = true;
    recommendedBrotliSettings = true;
    virtualHosts.${domain} = {
      enableACME = true;
      forceSSL = true;
      root = "${stateDir}/reports";
      extraConfig = ''
        index index.html;
        charset utf-8;
        add_header X-Content-Type-Options nosniff always;
      '';
      locations."/" = {
        extraConfig = ''
          try_files $uri $uri/ =404;
          add_header Cache-Control "public, max-age=300, must-revalidate" always;
          add_header X-Content-Type-Options nosniff always;
        '';
      };
      locations."~ /\\." = {
        extraConfig = "deny all;";
      };
    };
  };

  # Baseline history offsite. /root/borg/{ssh,passphrase} are hand-placed
  # (telemetry pattern); until they exist the job fails harmlessly.
  services.borgbackup.jobs.mageos-testing = {
    preHook = ''
      rm -rf /var/lib/borg-stage/baseline
      cp -r ${stateDir}/baseline /var/lib/borg-stage/baseline
      cp ${stateDir}/reports/runs.json /var/lib/borg-stage/ 2>/dev/null || true
    '';
    paths = [ "/var/lib/borg-stage" ];
    readWritePaths = [ "/var/lib/borg-stage" ];
    repo = "ssh://u627005@u627005.your-storagebox.de:23/./mageos-testing";
    encryption = {
      mode = "repokey-blake2";
      passCommand = "cat /root/borg/passphrase";
    };
    environment = {
      BORG_RSH = "ssh -i /root/borg/ssh -o StrictHostKeyChecking=accept-new";
    };
    compression = "auto,zstd";
    startAt = "*-*-* 05:45:00 UTC";
    prune.keep = {
      daily = 14;
      weekly = 8;
      monthly = 12;
    };
  };
}
