{ config, pkgs, lib, inputs, ... }:
let
  cressetView = inputs.self.packages.${pkgs.stdenv.hostPlatform.system}.cresset-view;
  authentikEnvironment = {
    AUTHENTIK_SECRET_KEY = "file://${config.sops.secrets."authentik/secret_key".path}";
    AUTHENTIK_POSTGRESQL__HOST = "/run/postgresql";
    AUTHENTIK_POSTGRESQL__NAME = "authentik";
    AUTHENTIK_POSTGRESQL__USER = "authentik";
    AUTHENTIK_POSTGRESQL__SSLMODE = "disable";
    AUTHENTIK_STORAGE__FILE__PATH = "/var/lib/authentik/media";
    AUTHENTIK_LISTEN__HTTP = "127.0.0.1:9000";
    AUTHENTIK_LISTEN__HTTPS = "127.0.0.1:9443";
    AUTHENTIK_LISTEN__METRICS = "127.0.0.1:9300";
    AUTHENTIK_LISTEN__DEBUG = "127.0.0.1:9900";
    AUTHENTIK_LISTEN__DEBUG_PY = "127.0.0.1:9901";
    AUTHENTIK_LISTEN__TRUSTED_PROXY_CIDRS = "127.0.0.0/8,::1/128";
    AUTHENTIK_OUTPOSTS__DISCOVER = "false";
    # Identifies the bootstrapped superuser. Not a secret, so it stays here rather than
    # in the env file below; on its own it does nothing, since authentik only runs the
    # bootstrap when a password, hash or token is also present.
    AUTHENTIK_BOOTSTRAP_EMAIL = "jelle@pingiun.com";
    HOME = "/var/lib/authentik";
  };
  # The worker binds listeners too, from the SAME AUTHENTIK_LISTEN__* settings as the
  # server. Sharing one environment therefore had the two units racing for the same
  # ports, and the worker usually won:
  #
  #   authentik-server: "listen tcp 127.0.0.1:9000: bind: address already in use"
  #
  # The server then fell back to HTTPS on 9443 and a unix socket under /dev/shm, which
  # PrivateTmp=true makes invisible to nginx. So nginx's proxy_pass to 9000 reached the
  # WORKER, which answers every path with 200 and an empty body -- a blank page at
  # auth.cresset.tools, and a 200 status that made it look healthy from the outside.
  #
  # Give the worker its own ports so the server deterministically owns the ones nginx
  # proxies to, rather than leaving it to whichever unit starts first.
  workerListenOverrides = {
    AUTHENTIK_LISTEN__HTTP = "127.0.0.1:9010";
    AUTHENTIK_LISTEN__HTTPS = "127.0.0.1:9453";
    AUTHENTIK_LISTEN__METRICS = "127.0.0.1:9310";
    AUTHENTIK_LISTEN__DEBUG = "127.0.0.1:9910";
    AUTHENTIK_LISTEN__DEBUG_PY = "127.0.0.1:9911";
  };
  authentikService = command: extraEnv: {
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "postgresql.service" "postgresql-setup.service" ];
    requires = [ "postgresql.service" "postgresql-setup.service" ];
    wants = [ "network-online.target" ];
    environment = authentikEnvironment // extraEnv;
    serviceConfig = {
      User = "authentik";
      Group = "authentik";
      ExecStart = "${pkgs.authentik}/bin/ak ${command}";
      Restart = "on-failure";
      RestartSec = "5s";
      StateDirectory = "authentik";
      WorkingDirectory = "/var/lib/authentik";
      UMask = "0077";
      PrivateTmp = true;
      ProtectHome = true;
      ProtectSystem = "strict";
      ReadWritePaths = [ "/var/lib/authentik" ];
      # The bootstrap credential, which CANNOT be passed the way AUTHENTIK_SECRET_KEY is:
      # authentik/core/setup/signals.py reads it with a plain os.getenv, so it gets none
      # of the `file://` expansion the config loader gives other settings. It has to
      # arrive as a literal value, hence an EnvironmentFile rendered by sops.
      EnvironmentFile = [ config.sops.templates."authentik-bootstrap.env".path ];
    };
  };
in
{
  imports = [
    inputs.sops-nix.nixosModules.sops
    # Root authorization for the CD workflow. `internal` was added to deploy.yml's matrix
    # when it was provisioned and this was missed, so every CD run since has failed on this
    # host alone with `Permission denied (publickey)` while the other four deployed fine —
    # a red job nobody was watching. The matrix and this import have to move together.
    ../../modules/ci-deploy-key.nix
    ./git-canonical.nix
    ./cresset-sync.nix
    ./backup.nix
  ];

  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
  boot.kernelParams = [ "console=tty1" "console=ttyS0,115200" ];
  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_scsi"
    "virtio_blk"
    "virtio_net"
    "ahci"
    "xhci_pci"
    "sd_mod"
    "sr_mod"
  ];

  networking = {
    hostName = "internal";
    usePredictableInterfaceNames = false;
    useDHCP = lib.mkDefault true;
    firewall = {
      enable = true;
      allowedTCPPorts = [ 22 80 443 ];
    };
  };

  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
  };
  users.mutableUsers = false;
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
  ];

  sops.defaultSopsFile = ../../secrets/internal.yaml;
  sops.age.sshKeyPaths = [ "/etc/ssh/ssh_host_ed25519_key" ];
  sops.secrets."authentik/secret_key" = {
    owner = "authentik";
    group = "authentik";
    mode = "0400";
  };

  # ---- Make the bootstrap unclaimable ----
  # authentik's initial-setup flow is unauthenticated by construction: it is what
  # creates the superuser, so on a publicly reachable instance whoever finds it first
  # owns the identity provider. The first `internal` sat in exactly that state, and the
  # journal showed Censys, zgrab and Tor exits sweeping it within hours of its
  # certificate appearing in the CT log.
  #
  # Restricting the flow at the proxy only narrows the race. Setting a bootstrap
  # credential removes it: akadmin gets this password the moment the database is
  # created, so the instance is never in a claimable state -- not on first boot, and
  # not on any future rebuild. The setup flow is then refused by authentik's own policy
  # rather than by us, and there is no window for a human to lose.
  #
  # This is the plaintext password rather than AUTHENTIK_BOOTSTRAP_PASSWORD_HASH (which
  # authentik also accepts, via `ak hash_password`). The hash would keep the plaintext
  # off the box entirely, but a human still has to type this at a login form, so it
  # would have to live in sops regardless -- and /run/secrets on this host already holds
  # the GitHub App private key, so a leak there is not survivable by hiding one value.
  #
  # Read it with:
  #   sops -d --extract '["authentik"]["bootstrap_password"]' secrets/internal.yaml
  sops.secrets."authentik/bootstrap_password" = {
    owner = "authentik";
    group = "authentik";
    mode = "0400";
  };
  sops.templates."authentik-bootstrap.env" = {
    owner = "authentik";
    group = "authentik";
    mode = "0400";
    content = ''
      AUTHENTIK_BOOTSTRAP_PASSWORD=${config.sops.placeholder."authentik/bootstrap_password"}
    '';
  };

  users.groups.authentik = { };
  users.users.authentik = {
    isSystemUser = true;
    group = "authentik";
    home = "/var/lib/authentik";
  };
  users.groups.cresset-view = { };
  users.users.cresset-view = {
    isSystemUser = true;
    group = "cresset-view";
    home = "/var/lib/cresset-view";
  };

  services.postgresql = {
    enable = true;
    package = pkgs.postgresql_17;
    settings.listen_addresses = lib.mkForce "";
    ensureDatabases = [ "authentik" ];
    ensureUsers = [
      { name = "authentik"; ensureDBOwnership = true; }
    ];
  };
  services.postgresqlBackup = {
    enable = true;
    databases = [ "authentik" ];
    startAt = "*-*-* 04:30:00 UTC";
  };

  systemd.services.authentik-server = authentikService "server" { };
  systemd.services.authentik-worker = authentikService "worker" workerListenOverrides;

  # The key cresset-view pushes a merge with. Owned by the service so it can read it, and by
  # nothing else: it is push access to the canonical repository.
  sops.secrets."cresset_view/merge_ssh_key" = {
    mode = "0400";
    owner = "cresset-view";
  };

  systemd.tmpfiles.rules = [
    "d /var/lib/cresset-view 0750 cresset-view cresset-view -"
    "d /var/lib/cresset-view/repository 0750 cresset-view cresset-view -"
    # The handshake with the canonical repository's push gate: cresset-view writes the list of
    # approved patch sets, and the update hook — running as `git` — reads it. It cannot live
    # under /var/lib/cresset-view, whose directory is owner-only so the hook could not traverse
    # it, and cresset-view has no business writing under /srv/git.
    #
    # SETGID (the leading 2) is load-bearing, not tidiness. Without it the directory is group
    # `git` but every file cresset-view creates inside it is group `cresset-view`, so the hook
    # cannot read the very file it exists to read — and the gate then refuses every push while
    # looking correctly configured. Measured on the box, not reasoned about: `runuser -u git --
    # test -r` said no. Setgid makes new files inherit the directory's group.
    "d /var/lib/cresset-review 2750 cresset-view git -"
    # And correct a file written before the setgid bit was set. `z` adjusts what is already
    # there without creating it, so this does not depend on cresset-view restarting after
    # tmpfiles — an ordering that is true today and is not worth relying on.
    "z /var/lib/cresset-review/approved 0640 cresset-view git -"
  ];

  systemd.services.cresset-view = {
    description = "Cresset jj repository viewer";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ];
    unitConfig.ConditionPathExists = "/var/lib/cresset-view/repository/current/.jj";
    environment.RUST_LOG = "cresset_view=info,tower_http=info";
    # RequiresMountsFor is a [Unit] directive. It sat in serviceConfig until systemd was
    # caught rejecting it -- "Unknown key 'RequiresMountsFor' in section [Service],
    # ignoring" -- so this guard was silently absent on every unit that declared it. The
    # /srv gating held anyway, on `requires = [ "srv.mount" ]`; this restores the second
    # layer it was always meant to have.
    unitConfig.RequiresMountsFor = "/srv";
    serviceConfig = {
      User = "cresset-view";
      Group = "cresset-view";
      # --sync-db surfaces cresset-sync's state: which projects are blocked, why, and how long
      # since a pass completed. It is optional and read READ-ONLY; if the worker is not
      # deployed, has not run, or the database cannot be opened, the panel reports itself
      # unavailable and the viewer carries on being a repository viewer.
      #
      # CAVEAT worth knowing before debugging this: the checkpoint database is in WAL mode, and
      # a read-only SQLite connection to a WAL database generally needs the `-shm` index, which
      # in turn wants write access to the directory. Reads may therefore fail while the worker
      # holds it, and that is deliberately survivable rather than fatal. If it proves flaky in
      # practice, the fix is for the worker to write a small JSON snapshot after each pass and
      # for this to read that instead — a plain file read has none of these problems.
      ExecStart = "${cressetView}/bin/cresset-view --repository /var/lib/cresset-view/repository/current --assets ${cressetView}/share/cresset-view --listen 127.0.0.1:9080 --sync-db /srv/sync/state.db --review-db /var/lib/cresset-view/review.db --approvals-file /var/lib/cresset-review/approved --merge-remote git@localhost:cresset.git --merge-ssh-key ${config.sops.secrets."cresset_view/merge_ssh_key".path}";
      Restart = "on-failure";
      RestartSec = "3s";
      NoNewPrivileges = true;
      PrivateTmp = true;
      PrivateDevices = true;
      ProtectHome = true;
      ProtectSystem = "strict";
      ReadOnlyPaths = [ "/var/lib/cresset-view/repository/current" "/srv/sync" ];
      # The review store and the approvals projection are the only things this service
      # writes. Narrow on purpose: the repository stays read-only above, so a defect in the
      # review code cannot reach the thing being reviewed.
      ReadWritePaths = [ "/var/lib/cresset-view" "/var/lib/cresset-review" ];
      # /srv is a nofail Cloud Volume, so the viewer must not be held up by it: the sync panel
      # degrades on its own if the path is absent.
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
    };
  };

    # Keep the viewer's repository current.
    #
    # There was no refresh at all. The directory was created once during provisioning and the
    # viewer served that snapshot unchanged for a week: on 2026-08-08 it still ended at
    # "feat(infra): rebuild internal" from 1 August while canonical `main` had moved on by
    # hundreds of commits. The `<operation-id>` directory name and the `current` symlink show
    # a snapshot-and-swap refresh was intended; only the naming ever got built.
    #
    # This mattered beyond stale history. cresset-sync's conflict banner links to
    # `?revision=<conflict commit>`, and a commit made after the snapshot is not in it -- so for
    # a week every escalation link pointed at a revision the viewer could not resolve. Telegram
    # said a human was needed and the link went nowhere.
    #
    # Fetching in place rather than snapshot-and-swap: it is a 0.6s local fetch against the bare
    # repo on the same disk, where a fresh clone is 347M of churn, and the viewer loads the
    # repository per request so a new operation is visible immediately with no restart. The
    # `current` symlink stays because the unit path depends on it.
    systemd.services.cresset-view-refresh = {
      description = "Fetch the canonical monorepo into the cresset-view repository";
      after = [ "srv.mount" ];
      # jj shells out to `git` for parts of the git backend, and a systemd unit has no PATH of
      # its own. Without this it fails with "Could not execute the git process, found in the OS
      # path 'git'" -- which names the binary but not the reason, and looks like a missing
      # package rather than a missing PATH.
      path = [ pkgs.git ];
      unitConfig.RequiresMountsFor = "/srv";
      serviceConfig = {
        Type = "oneshot";
        User = "cresset-view";
        Group = "cresset-view";
        # jj wants somewhere for a per-repo config; without HOME it errors on a path under a
        # home directory that does not exist and gives up.
        Environment = [ "JJ_CONFIG=/dev/null" "HOME=/var/lib/cresset-view" ];
        ExecStart = pkgs.writeShellScript "cresset-view-refresh" ''
          set -eu
          repo=/var/lib/cresset-view/repository/current
          jj="${pkgs.jujutsu}/bin/jj"
          if [ ! -d "$repo/.jj" ]; then
            # Self-healing: a unit that only works once someone has made the directory by hand
            # is not a fix for a directory nobody remembered to keep current.
            target=/var/lib/cresset-view/repository/bootstrap
            rm -rf "$target"
            "$jj" git clone --colocate /srv/git/cresset.git "$target"
            ln -sfn "$target" "$repo"
          fi
          # Over the local path, not the SSH remote the clone was made with: same disk, no
          # network, and no key for the `git` account that exists only to serve SSH.
          "$jj" -R "$repo" git remote set-url canonical /srv/git/cresset.git 2>/dev/null || true
          "$jj" -R "$repo" git fetch --remote canonical
          # Patch sets, which `jj git fetch` does not bring: it fetches refs/heads/* into
          # remote bookmarks, and review patch sets live at refs/changes/<change-id>/<n>.
          #
          # They are OUTSIDE refs/heads deliberately. Importing them as bookmarks makes every
          # version of a change a visible commit sharing one change id, and jj then refuses to
          # resolve it -- `Change ID is divergent`. That would break the identity the whole
          # review design rests on, in the one repository that most needs to resolve it. Kept
          # out of jj's namespace, the change id stays addressable and the old patch sets are
          # still there to read and diff.
          #
          # No --prune: patch sets are append-only history, and a superseded one is exactly
          # what a reviewer comes back for.
          ${pkgs.git}/bin/git -C "$repo" fetch --quiet canonical \
            "+refs/changes/*:refs/changes/*"
        '';
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ "/var/lib/cresset-view" ];
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictNamespaces = true;
        LockPersonality = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [ "@system-service" "~@privileged" ];
      };
    };

    systemd.timers.cresset-view-refresh = {
      description = "Keep the cresset-view repository within a couple of minutes of canonical";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        # Tighter than the worker's 5-minute reconcile: the viewer is where someone looks AFTER
        # being told something needs them, so it should not be the slower of the two.
        OnCalendar = "*:0/2";
        Persistent = true;
        RandomizedDelaySec = "20s";
      };
    };


  # git and jujutsu are here for the OPERATOR, not for any service: every unit that
  # needs them already carries its own `path`, and cresset-sync's wrapper pins its own
  # copies deliberately. But this is the box that hosts the canonical monorepo remote,
  # and inspecting it during bring-up (`show-ref`, `cat-file`, `log`) meant hunting the
  # binary out of /nix/store by hand — on the one host where reaching for git is the
  # obvious thing to do.
  # claude-code carries an unfree licence. Allow that ONE package by name rather than
  # setting allowUnfree globally: this flake has no unfree packages anywhere else, and a
  # blanket flag would silently permit the next one too.
  nixpkgs.config.allowUnfreePredicate = pkg: lib.getName pkg == "claude-code";

  # claude-code is here so `claude auth login` can be run once as the cresset-sync user
  # (see cresset-sync.nix). Being on the PATH does NOT enable the resolver: the worker
  # only dispatches an agent when `run` is given --agent-command, which ExecStart
  # deliberately omits. Installing it and enabling it stay separate decisions.
  environment.systemPackages = [ cressetView pkgs.git pkgs.jujutsu pkgs.claude-code ];

  security.acme = {
    acceptTerms = true;
    defaults.email = "jelle@pingiun.com";
  };

  services.nginx = {
    enable = true;
    recommendedOptimisation = true;
    recommendedTlsSettings = true;
    recommendedGzipSettings = true;
    recommendedProxySettings = true;

    # Log how long each request took, split between us and the network.
    #
    # Added after an approval in cresset-view reportedly took 30 seconds while every
    # server-side component measured under 110ms end to end — the handler at 2ms, the
    # Authentik forward-auth subrequest at 110ms, every API endpoint under 41ms. With only
    # the default `combined` format there was no way to tell whether the time was ours, and
    # a slow request behind forward-auth is exactly the thing that is hard to attribute
    # afterwards.
    #
    # `$request_time` is measured from the first byte read from the client to the last byte
    # written to it; `$upstream_response_time` is what the proxied service took. The two
    # together separate a slow application from a slow client connection: if request_time is
    # 30s and upstream_response_time is 0.01s, the time was spent on the wire, not here.
    # `$upstream_response_time` carries one entry per upstream, so an auth_request subrequest
    # shows up as a second figure — which is the point, since that is a second round trip
    # every request pays.
    commonHttpConfig = ''
      log_format timed '$remote_addr - $remote_user [$time_local] "$request" '
                       '$status $body_bytes_sent "$http_referer" "$http_user_agent" '
                       'request_time=$request_time upstream_time=$upstream_response_time';
      access_log /var/log/nginx/access.log timed;
    '';

    # Unknown Host -> drop the connection. This IP is recycled: nsk-test.werktoej.dk,
    # a previous tenant's name, still resolves here, so third-party traffic arrives
    # under hostnames we do not serve. Without a default server those requests fall
    # through to the FIRST vhost, which is the identity provider — they were landing
    # on Authentik. 444 closes the connection without a response.
    virtualHosts."_" = {
      default = true;
      rejectSSL = true;
      locations."/".return = "444";
    };

    virtualHosts."auth.cresset.tools" = {
      enableACME = true;
      forceSSL = true;
      locations."/" = {
        proxyPass = "http://127.0.0.1:9000";
        proxyWebsockets = true;
      };

      # Authentik's initial-setup flow is UNAUTHENTICATED by construction: it is the
      # bootstrap that creates the superuser, so whoever reaches it first owns the
      # instance. Between a host coming up and someone completing that flow, a public
      # identity provider is claimable by anyone — and this box was being swept by
      # Censys, zgrab and Tor exits within hours of its certificate hitting the CT log.
      #
      # A longer prefix wins in nginx, so these two locations override "/" above and
      # confine the flow — both its UI entry point and the API the UI drives it
      # through — to loopback. Run the setup over an SSH tunnel:
      #
      #   ssh -L 9000:127.0.0.1:9000 root@internal.cresset.tools
      #   open http://localhost:9000/if/flow/initial-setup/
      #
      # This is a stop-gap for a race, not a fix for it. The fix is to never leave the
      # instance claimable: set AUTHENTIK_BOOTSTRAP_PASSWORD from sops so akadmin has a
      # password the moment the database is created, which retires this flow for good.
      locations."/if/flow/initial-setup/" = {
        proxyPass = "http://127.0.0.1:9000";
        extraConfig = ''
          allow 127.0.0.1;
          allow ::1;
          deny all;
        '';
      };
      locations."/api/v3/flows/executor/initial-setup/" = {
        proxyPass = "http://127.0.0.1:9000";
        extraConfig = ''
          allow 127.0.0.1;
          allow ::1;
          deny all;
        '';
      };
    };

    virtualHosts."code.cresset.tools" = {
      enableACME = true;
      forceSSL = true;

      locations."/" = {
        proxyPass = "http://127.0.0.1:9080";
        proxyWebsockets = true;
        extraConfig = ''
          auth_request /outpost.goauthentik.io/auth/nginx;
          error_page 401 = @goauthentik_proxy_signin;

          auth_request_set $auth_cookie $upstream_http_set_cookie;
          add_header Set-Cookie $auth_cookie;

          auth_request_set $authentik_username $upstream_http_x_authentik_username;
          auth_request_set $authentik_groups $upstream_http_x_authentik_groups;
          auth_request_set $authentik_email $upstream_http_x_authentik_email;

          # The fail-closed check lives in cresset-view, NOT here. It was here, as
          #   if ($authentik_username = "") { return 403; }
          # which cannot work: `if`/`return` belongs to the rewrite phase and `auth_request`
          # to the access phase that runs after it, so the guard read the identity before the
          # outpost had ever been called, found it empty every time, and refused everyone
          # unconditionally. A wall, not a gate — and indistinguishable from a working gate
          # for as long as nobody could get in.
          #
          # nginx's job is therefore only to ask the outpost and forward what it answers.
          # cresset-view refuses any request whose identity header is absent or empty, which
          # is phase-independent and is the better place regardless: the service must not
          # trust the proxy to have authenticated anyone, only to have reported who it is.

          proxy_set_header X-authentik-username $authentik_username;
          proxy_set_header X-authentik-groups $authentik_groups;
          proxy_set_header X-authentik-email $authentik_email;
        '';
      };

      # Deliberately minimal. This carried proxyWebsockets plus
      # `proxy_pass_request_body off` and `proxy_set_header Content-Length ""`, and every
      # request to this prefix was answered 400 by nginx WITHOUT reaching the outpost —
      # no nginx error log, and nothing in authentik's. The same upstream path through
      # auth.cresset.tools, which has none of those directives, reached the outpost fine.
      #
      # `$host` rather than `$http_host`: over HTTP/2 there is no literal Host header, so
      # $http_host can be empty while $host is always the matched server name.
      locations."/outpost.goauthentik.io" = {
        # NO URI part. With `proxy_pass .../outpost.goauthentik.io` nginx rewrites the
        # matched prefix, and every request to this prefix was answered 400 by nginx without
        # ever reaching the outpost — no entry in nginx's error log or authentik's. The same
        # path through auth.cresset.tools, whose `location /` proxies with no URI part,
        # reached the outpost fine. Passing the path through unchanged matches that.
        proxyPass = "http://127.0.0.1:9000";
        # NO `proxy_set_header Host` here. services.nginx.recommendedProxySettings already
        # sets it, and nginx does not deduplicate: declaring it again emits the header TWICE,
        # which Go's net/http rejects with 400 at the protocol layer — before any handler, so
        # authentik logged nothing and nginx logged nothing either. Every request to this
        # prefix failed that way, and the forward-auth with it. auth.cresset.tools was
        # unaffected only because its location adds no Host of its own.
        extraConfig = ''
          proxy_set_header X-Original-URL $scheme://$host$request_uri;
        '';
      };

      # The forward-auth subrequest, as its own exact-match location.
      #
      # `proxy_pass_request_body off` is what makes a POST here take 60ms instead of 30
      # SECONDS. Without it nginx forwards the client body into the auth subrequest and never
      # consumes it on the main request, so after answering it lingering-closes -- reading and
      # discarding client data for up to `lingering_time`, whose default is exactly 30s.
      #
      # Measured, and it reproduces without a session: POST with an empty body 0.06s, POST with
      # a 33-byte body 30.06s, identically over HTTP/1.1 and HTTP/2 and regardless of
      # Connection: close. In the access log it reads `request_time=30.010
      # upstream_time=0.009` -- nine milliseconds of application, thirty seconds of nginx.
      #
      # This affects EVERY POST behind this forward-auth, not just cresset-view's approvals; it
      # only surfaced now because nothing here had a write endpoint before. The empty
      # Content-Length goes with it: the auth server must not be told to expect a body that is
      # deliberately not being sent.
      #
      # Exact match (`=`) so it wins over the `/outpost.goauthentik.io` prefix above, which
      # still serves the interactive sign-in endpoints and does need bodies.
      locations."= /outpost.goauthentik.io/auth/nginx" = {
        proxyPass = "http://127.0.0.1:9000";
        extraConfig = ''
          internal;
          proxy_pass_request_body off;
          proxy_set_header Content-Length "";
          proxy_set_header X-Original-URL $scheme://$host$request_uri;
        '';
      };

      locations."@goauthentik_proxy_signin".extraConfig = ''
        internal;
        add_header Set-Cookie $auth_cookie;
        return 302 /outpost.goauthentik.io/start?rd=$scheme://$host$request_uri;
      '';
    };
  };

  services.journald.extraConfig = ''
    SystemMaxUse=1G
    MaxRetentionSec=2week
  '';

  system.stateVersion = "25.11";
}
