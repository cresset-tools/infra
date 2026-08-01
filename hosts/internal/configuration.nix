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
  authentikService = command: {
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" "postgresql.service" "postgresql-setup.service" ];
    requires = [ "postgresql.service" "postgresql-setup.service" ];
    wants = [ "network-online.target" ];
    environment = authentikEnvironment;
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
    ./git-canonical.nix
    ./cresset-sync.nix
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

  systemd.services.authentik-server = authentikService "server";
  systemd.services.authentik-worker = authentikService "worker";

  systemd.tmpfiles.rules = [
    "d /var/lib/cresset-view 0750 cresset-view cresset-view -"
    "d /var/lib/cresset-view/repository 0750 cresset-view cresset-view -"
  ];

  systemd.services.cresset-view = {
    description = "Cresset jj repository viewer";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ];
    unitConfig.ConditionPathExists = "/var/lib/cresset-view/repository/current/.jj";
    environment.RUST_LOG = "cresset_view=info,tower_http=info";
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
      ExecStart = "${cressetView}/bin/cresset-view --repository /var/lib/cresset-view/repository/current --assets ${cressetView}/share/cresset-view --listen 127.0.0.1:9080 --sync-db /srv/sync/state.db";
      Restart = "on-failure";
      RestartSec = "3s";
      NoNewPrivileges = true;
      PrivateTmp = true;
      PrivateDevices = true;
      ProtectHome = true;
      ProtectSystem = "strict";
      ReadOnlyPaths = [ "/var/lib/cresset-view/repository/current" "/srv/sync" ];
      # /srv is a nofail Cloud Volume, so the viewer must not be held up by it: the sync panel
      # degrades on its own if the path is absent.
      RequiresMountsFor = "/srv";
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
    };
  };

  # git and jujutsu are here for the OPERATOR, not for any service: every unit that
  # needs them already carries its own `path`, and cresset-sync's wrapper pins its own
  # copies deliberately. But this is the box that hosts the canonical monorepo remote,
  # and inspecting it during bring-up (`show-ref`, `cat-file`, `log`) meant hunting the
  # binary out of /nix/store by hand — on the one host where reaching for git is the
  # obvious thing to do.
  environment.systemPackages = [ cressetView pkgs.git pkgs.jujutsu ];

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

          # FAIL CLOSED. `auth_request` alone is not a gate here: Authentik's embedded
          # outpost answers 200 for a host it has no provider configured for, so between
          # this vhost going live and the proxy provider being created in the Authentik
          # UI, every anonymous request was approved. That is not hypothetical — it is
          # what happened on first provision, and the whole private monorepo was served
          # to the internet until the viewer was stopped.
          #
          # A 200 from the outpost is therefore not proof of authentication; an identity
          # header is. Authentik only sets X-authentik-username once a request has really
          # been authorised, so an empty one means "not authenticated" or "not configured"
          # — and both must be refused. An unconfigured gate now returns 403 instead of
          # the repository.
          if ($authentik_username = "") {
            return 403 "authentication is not configured for this host";
          }

          proxy_set_header X-authentik-username $authentik_username;
          proxy_set_header X-authentik-groups $authentik_groups;
          proxy_set_header X-authentik-email $authentik_email;
        '';
      };

      locations."/outpost.goauthentik.io" = {
        proxyPass = "http://127.0.0.1:9000/outpost.goauthentik.io";
        proxyWebsockets = true;
        extraConfig = ''
          proxy_set_header Host $host;
          proxy_set_header X-Original-URL $scheme://$http_host$request_uri;
          proxy_pass_request_body off;
          proxy_set_header Content-Length "";
          auth_request_set $auth_cookie $upstream_http_set_cookie;
          add_header Set-Cookie $auth_cookie;
        '';
      };

      locations."@goauthentik_proxy_signin".extraConfig = ''
        internal;
        add_header Set-Cookie $auth_cookie;
        return 302 /outpost.goauthentik.io/start?rd=$scheme://$http_host$request_uri;
      '';
    };
  };

  services.journald.extraConfig = ''
    SystemMaxUse=1G
    MaxRetentionSec=2week
  '';

  system.stateVersion = "25.11";
}
