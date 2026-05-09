# nginx configuration for the origin's two vhosts.
#
#   index.bougie.tools  → /srv/index/                  (symlink to a versioned dir)
#   blobs.bougie.tools  → /srv/blobs/                  (content-addressed tarballs)
#
# Cache policy follows DISTRIBUTION.md:
#   - /index.json (root): max-age=30, must-revalidate, ETag — small, frequent.
#   - everything else under /index/: public, max-age=31536000, immutable —
#     content-addressed via the section-hash chain, never changes at a URL.
#   - blobs/: same long-immutable cache; sha256 path → bytes never change.
{ config, pkgs, lib, ... }:
{
  security.acme = {
    acceptTerms = true;
    defaults.email = "jelle@pingiun.com";
  };

  services.nginx = {
    enable = true;

    recommendedGzipSettings = true;
    recommendedOptimisation = true;
    recommendedProxySettings = false;
    recommendedTlsSettings = true;
    recommendedBrotliSettings = true;

    # nginx must follow the /srv/index symlink to the current versioned
    # tree. NixOS defaults disable_symlinks off (the safe-but-permissive
    # mode); keep that.

    virtualHosts."index.bougie.tools" = {
      enableACME = true;
      forceSSL = true;
      http3 = true;
      kTLS = true;

      root = "/srv/index";

      # Long-cache everything by default; the root index.json overrides
      # below. JSON is the only content type here.
      extraConfig = ''
        default_type application/json;
        charset utf-8;
        add_header X-Content-Type-Options nosniff always;

        # Default: content-addressed via the hash chain → never changes
        # at a given URL → infinite immutable.
        add_header Cache-Control "public, max-age=31536000, immutable" always;
        etag on;
      '';

      locations."= /index.json" = {
        # Short-TTL revalidation for the only mutable file in the tree.
        # ETag means most refetches are 304s; max-age keeps misbehaving
        # caches honest.
        extraConfig = ''
          add_header Cache-Control "public, max-age=30, must-revalidate" always;
          etag on;
        '';
      };

      locations."= /index.json.sig" = {
        extraConfig = ''
          add_header Cache-Control "public, max-age=30, must-revalidate" always;
          add_header X-Content-Type-Options nosniff always;
          etag on;
        '';
      };

      # Belt-and-braces: deny dotfiles even though no .* paths should exist.
      locations."~ /\\." = {
        extraConfig = "deny all;";
      };
    };

    virtualHosts."blobs.bougie.tools" = {
      enableACME = true;
      forceSSL = true;
      http3 = true;
      kTLS = true;

      root = "/srv";

      # /srv/blobs/<prefix>/<sha256> is the only path served here.
      # No directory listings, no fallback, no anything else.
      locations."/blobs/" = {
        extraConfig = ''
          # Tarballs are zstd-compressed; gzip on top is a waste.
          gzip off;
          brotli off;

          # Content-addressed by sha256: bytes at a URL are immutable.
          add_header Cache-Control "public, max-age=31536000, immutable" always;
          add_header X-Content-Type-Options nosniff always;

          # The CLI accepts these two; nginx's defaults match either.
          types { application/zstd zst; application/octet-stream ""; }
          default_type application/octet-stream;

          autoindex off;
          try_files $uri =404;
        '';
      };

      # Anything outside /blobs/ is 404. Keeps misconfiguration contained.
      locations."/" = {
        extraConfig = ''
          return 404;
        '';
      };
    };
  };
}
