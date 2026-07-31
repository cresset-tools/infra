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

    virtualHosts."auth.cresset.tools" = {
      enableACME = true;
      forceSSL = true;
      locations."/" = {
        proxyPass = "http://127.0.0.1:9000";
        proxyWebsockets = true;
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
