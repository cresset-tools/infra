# nginx configuration for the origin's two vhosts.
#
#   index.bougie.tools  → /srv/index/                  (snapshot-model flat tree)
#   blobs.bougie.tools  → /srv/blobs/                  (content-addressed tarballs)
#
# /srv/index/ is a flat directory written by the publish pipeline
# (cresset-tools/php-build-standalone scripts/rsync-publish-tree.sh).
# Layout (DISTRIBUTION.md "Server layout"):
#
#   /srv/index/
#     index.json                                       # mutable root (replaced atomically per publish)
#     index.json.sig                                   # mutable signature
#     versions/<V>/targets/<target>/sections/...       # immutable per-publish snapshot
#     targets/<target>/manifests/...                   # immutable, content-addressed by tag
#
# Cache policy:
#   - /index.json + /index.json.sig: max-age=30, must-revalidate, ETag.
#     The only mutable URLs in the protocol; revalidations are mostly
#     304s thanks to ETag.
#   - everything else: public, max-age=31536000, immutable — section
#     URLs include the publish version, manifest URLs embed the tag,
#     so the bytes at every URL are immutable for life.
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

    virtualHosts."index.bougie.tools" = {
      enableACME = true;
      forceSSL = true;

      root = "/srv/index";

      # Long-cache everything by default; the root index.json + .sig
      # overrides below. JSON is the only content type here.
      extraConfig = ''
        default_type application/json;
        charset utf-8;
        add_header X-Content-Type-Options nosniff always;

        # Default: URL-immutable (versioned section paths,
        # tag-embedded manifest paths) → infinite immutable cache.
        add_header Cache-Control "public, max-age=31536000, immutable" always;
        etag on;
      '';

      locations."= /index.json" = {
        # Short-TTL revalidation for the only mutable URL in the tree.
        # ETag means most refetches are 304s; max-age keeps misbehaving
        # caches honest.
        #
        # add_header in nginx REPLACES parent add_headers, it doesn't
        # accumulate — so we must re-emit every header we want here, not
        # just the one we're overriding. gixy enforces this at build time.
        extraConfig = ''
          add_header Cache-Control "public, max-age=30, must-revalidate" always;
          add_header X-Content-Type-Options nosniff always;
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

          # Blob filenames are bare sha256 hashes (no extension), so the
          # default_type fallback governs what's served — application/
          # octet-stream is the right answer for opaque tarballs.
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
