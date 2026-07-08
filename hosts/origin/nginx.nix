# nginx configuration for the origin's vhosts.
#
#   index.bougie.tools     → /srv/index/                  (snapshot-model flat tree)
#   blobs.bougie.tools     → /srv/blobs/                  (content-addressed tarballs)
#   releases.bougie.tools  → /srv/releases/               (bougie binary mirror)
#   bougie.tools           → ./bougie (homepage)      (+ install.sh / install.ps1 redirects)
#   cresset.tools          → ./cresset (homepage)     (umbrella brand)
#   modulargento.cresset.tools → /srv/modulargento/          (Composer repo)
#   www.{bougie,cresset}.tools → 301 → apex                (globalRedirect)
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

    # releases.bougie.tools — the bougie binary distribution mirror,
    # standing in for an R2 bucket. dist's release.yml CI uploads two
    # things into /srv/releases/ via rsync over SSH as the `deploy`
    # user (see configuration.nix for the user definition and
    # bougie's .github/workflows/publish-mirror.yml for the publish
    # side):
    #
    #   /srv/releases/github/bougie/releases/download/<tag>/<file>
    #       Per-tag immutable archives + installers + checksums. The
    #       path shape mirrors the GitHub Releases URL so dist's
    #       generated installer can fall back from this mirror to GH
    #       without rewriting paths.
    #
    #   /srv/releases/installers/bougie/latest/bougie-installer.{sh,ps1}
    #       Rolling pointer that `curl -LsSf bougie.tools/install.sh
    #       | sh` resolves to (via the apex redirect below). Short
    #       cache so a new release shows up quickly; the installer
    #       itself pins exact archive sha256s, so even a stale
    #       installer can't tamper with what it ends up running.
    #
    # No directory listings anywhere. Anything outside the two
    # documented prefixes 404s — matches blobs.bougie.tools'
    # "misconfiguration contained" stance.
    virtualHosts."releases.bougie.tools" = {
      enableACME = true;
      forceSSL = true;

      root = "/srv/releases";

      # Common headers for both prefixes; per-location blocks override
      # Cache-Control. Per nginx semantics, `add_header` in a child
      # block REPLACES (not extends) parent add_headers — every
      # location below re-emits the security headers it wants.
      extraConfig = ''
        add_header X-Content-Type-Options nosniff always;
        charset utf-8;
      '';

      # Versioned artifacts: URL embeds the tag, bytes never change.
      # Year-long immutable cache. Includes archive tarballs/zips,
      # the per-archive *.sha256 sidecars, the combined sha256.sum,
      # and the per-release copies of the installer scripts that
      # dist also uploads to the GitHub Release.
      locations."/github/" = {
        extraConfig = ''
          gzip off;
          brotli off;

          add_header Cache-Control "public, max-age=31536000, immutable" always;
          add_header X-Content-Type-Options nosniff always;
          default_type application/octet-stream;

          # .sh / .ps1 / .sha256 are text; let nginx infer JSON too in
          # case dist starts emitting a manifest URL here. types{} below
          # adds the few content types the default mime list misses for
          # bare-stem files.
          types {
            application/x-sh           sh;
            text/plain                 ps1 sha256;
            application/json           json;
            application/gzip           tar.gz tgz;
            application/zip            zip;
          }

          autoindex off;
          try_files $uri =404;
        '';
      };

      # Rolling installers — the `curl ... | sh` entry point. Five
      # minute cache (matches the uv/R2 setup the bougie spec started
      # from) so a freshly cut release reaches users without being
      # cached for hours, but each request doesn't bypass CDN
      # entirely.
      locations."/installers/" = {
        extraConfig = ''
          gzip on;
          gzip_types application/x-sh text/plain;
          brotli off;

          add_header Cache-Control "public, max-age=300, must-revalidate" always;
          add_header X-Content-Type-Options nosniff always;

          types {
            application/x-sh           sh;
            text/plain                 ps1;
          }

          autoindex off;
          try_files $uri =404;
        '';
      };

      # Everything else 404s. Belt-and-braces match for the
      # blobs.bougie.tools layout.
      locations."/" = {
        extraConfig = ''
          return 404;
        '';
      };

      locations."~ /\\." = {
        extraConfig = "deny all;";
      };
    };

    # bougie.tools apex — static homepage + the installer redirects.
    # `curl -LsSf https://bougie.tools/install.sh | sh` is the public
    # one-liner; the exact-match install.sh / install.ps1 locations take
    # precedence over the catch-all `/`, so the one-liner keeps working
    # alongside the homepage. wick.sh / wick.ps1 / magequery.sh /
    # magequery.ps1 / jibs.sh are the same trick for the other tools
    # (short aliases for their releases-mirror installers; jibs has no
    # .ps1 — it ships no Windows binaries).
    # The site is a single self-contained `index.html` (no external
    # assets) in ./bougie, copied into the store at build time — edit it
    # and `nix run .#switch -- origin <ip>`.
    virtualHosts."bougie.tools" = {
      enableACME = true;
      forceSSL = true;

      root = ./bougie;

      # Server-level defaults. Per nginx semantics a child `add_header`
      # REPLACES (not extends) these, so the `/` location below re-emits
      # the security header it wants (gixy enforces this at build time).
      extraConfig = ''
        index index.html;
        charset utf-8;
        add_header X-Content-Type-Options nosniff always;
      '';

      locations."= /install.sh" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/bougie/latest/bougie-installer.sh;
        '';
      };

      locations."= /install.ps1" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/bougie/latest/bougie-installer.ps1;
        '';
      };

      locations."= /wick.sh" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/wick/latest/wick-installer.sh;
        '';
      };

      locations."= /wick.ps1" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/wick/latest/wick-installer.ps1;
        '';
      };

      locations."= /magequery.sh" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/magequery/latest/magequery-installer.sh;
        '';
      };

      locations."= /magequery.ps1" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/magequery/latest/magequery-installer.ps1;
        '';
      };

      locations."= /jibs.sh" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/jibs/latest/jibs-installer.sh;
        '';
      };

      locations."= /sconce.sh" = {
        extraConfig = ''
          return 301 https://releases.bougie.tools/installers/sconce/latest/sconce-installer.sh;
        '';
      };

      # Public telemetry policy + dashboard. The consent prompts and
      # TELEMETRY.md print bougie.tools/telemetry; the dashboard itself
      # is served by the telemetry host (hosts/telemetry/), which owns
      # the aggregate data.
      locations."= /telemetry" = {
        extraConfig = ''
          return 301 https://telemetry.bougie.tools/;
        '';
      };

      # Static homepage. One self-contained file, so a short revalidating
      # cache keeps edits visible without bypassing the CDN. Unknown
      # paths 404 against the site root rather than redirecting.
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

    # cresset.tools apex — the umbrella-brand homepage (bougie's parent).
    # Same static-site shape as bougie.tools, served from ./cresset.
    # No installer redirects here. (DNS: cresset.tools must A → this box,
    # via Cloudflare, before ACME can issue — see README "Point DNS".)
    virtualHosts."cresset.tools" = {
      enableACME = true;
      forceSSL = true;

      root = ./cresset;

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

    # modulargento.cresset.tools — Composer package repository for the
    # modulargento fork. Static file host: satis-generated packages.json
    # plus zip archives under /packages/. Published by rsync from CI or
    # manually as the `deploy` user into /srv/modulargento/.
    virtualHosts."modulargento.cresset.tools" = {
      enableACME = true;
      forceSSL = true;

      root = "/srv/modulargento";

      extraConfig = ''
        charset utf-8;
        autoindex off;
        add_header X-Content-Type-Options nosniff always;
        add_header Cache-Control "public, max-age=300, must-revalidate" always;
      '';

      locations."/" = {
        extraConfig = ''
          try_files $uri $uri/ =404;
          add_header Cache-Control "public, max-age=300, must-revalidate" always;
          add_header X-Content-Type-Options nosniff always;
        '';
      };

      # The dist zips live at version-stable URLs (…/module-foo-3.0.0.zip), but
      # the modulargento release republishes the SAME lockstep version (3.0.0)
      # with new content on every rebuild — so the bytes at a given URL are NOT
      # immutable (unlike the mage-os mirror, where a release version is final).
      # Marking them immutable lets any HTTP cache (plain Composer, a proxy, a
      # browser) serve a stale zip forever after a republish. Use a short TTL +
      # must-revalidate so caches revalidate against the etag/last-modified
      # nginx already emits (cheap 304s when unchanged, fresh bytes when not).
      locations."/packages/" = {
        extraConfig = ''
          add_header Cache-Control "public, max-age=300, must-revalidate" always;
          add_header X-Content-Type-Options nosniff always;
        '';
      };

      locations."~ /\\." = {
        extraConfig = "deny all;";
      };
    };

    # www.* → apex 301s (preserving path) via the NixOS nginx
    # globalRedirect option. Each needs its own DNS record + ACME cert,
    # so point `www.bougie.tools` / `www.cresset.tools` at this box too.
    virtualHosts."www.bougie.tools" = {
      enableACME = true;
      forceSSL = true;
      globalRedirect = "bougie.tools";
    };

    virtualHosts."www.cresset.tools" = {
      enableACME = true;
      forceSSL = true;
      globalRedirect = "cresset.tools";
    };
  };
}
