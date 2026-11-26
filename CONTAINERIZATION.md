# Phase 3 — containerize the licensing demo (Nix-built images)

How the [DEMO_PLAN.md](DEMO_PLAN.md) demo (sconce "Bougie Repo" + a Magento store)
gets packaged for `hosts/demo`. Goal: **minimal, deterministic, easy to upgrade**
— our own two apps ship as **Nix-built OCI images** (`dockerTools`), the datastores
run as **native NixOS services**, and a nixpkgs bump upgrades the whole runtime.

> Status: **implemented & live.** Everything below is wired into the flake and
> serving on `hosts/demo`. Where the design and the as-built config drifted, it's
> noted inline — `hosts/demo/configuration.nix` is the authority.
>
> **sconce left the box on 2026-11-25.** The `cresset` org was moved to
> [bougierepo.com](https://bougierepo.com) with `sconce org-export` /
> `org-import`, and the store's licensing module now talks to it there. The
> demo box runs only the `magento` container: the `sconce` and `sconce-ui`
> containers, Postgres and `admin.bougie.tools` are gone, and
> `repo.bougie.tools` is a 301 redirect to `bougierepo.com` (same path). The
> sconce sections below are kept as the history of how it ran here.

## Decisions (locked)

- **Our apps → Nix-built OCI images** (`dockerTools.buildLayeredImage`), loaded via
  `virtualisation.oci-containers … imageFile` (no registry). `hosts/demo` will be the
  flake's **first** oci-containers + dockerTools consumer.
- **Magento app tree → a non-Nix layer on top of the Nix runtime image.** The image
  is a minimal, upgradeable php-fpm+nginx runtime built by Nix; the Magento
  code/vendor/generated tree is **host state**, produced by the proven Phase-2
  `bougie` build (production-compiled) and delivered as a seed — not rebuilt from
  source in Nix. (Chosen over a full `php.buildComposerProject` build, which would
  have to solve private-repo auth in a fixed-output derivation + Magento's
  di:compile/static-deploy in-derivation.)
- **Datastores → classic `services.*` NixOS modules**, run natively. *Not* NixOS
  "modular services": that framework (`system.services.*`) is merged but experimental
  in 25.11, its only backend is systemd anyway, and **none of Postgres / MariaDB /
  OpenSearch / Redis exist as modular services** at our pinned nixpkgs (only a ~6
  pilot set like ghostunnel). The classic modules are all present and mature.
- **TLS** stays the repo convention: host **nginx + `security.acme` HTTP-01**
  (Cloudflare grey-cloud), reverse-proxying to **loopback** container ports.

## Architecture

As it runs today:

```
      demo.bougie.tools ─┐        repo.bougie.tools ─► 301 https://bougierepo.com
                         ▼
  ┌──────────────  hosts/demo  (Hetzner CX33, x86_64, NixOS) ─────────┐
  │  nginx + security.acme (HTTP-01)   →  TLS + vhost routing          │
  │        │  proxy 127.0.0.1:8081                                     │
  │  ┌── oci-container: magento ──────┐                                │
  │  │ Nix img: php84-fpm + nginx     │── licensing API ──► bougierepo.com
  │  │ + app tree (host bind-mount,   │                                │
  │  │   seeded) + env (secret)       │                                │
  │  └───────┬────────┬────────┬──────┘                                │
  │   native │ native │ native │                                       │
  │  ┌─ mariadb ─┐ ┌ opensearch ┐ ┌ redis ┐                            │
  │  services.mysql  services.    services.redis                       │
  │  (pkg=mariadb)   opensearch                                        │
  └────────────────────────────────────────────────────────────────────┘
```

The container reaches the native datastores over the host loopback
(`--network=host`).

Until 2026-11-25 the box also ran a `sconce` container behind
`repo.bougie.tools` (`127.0.0.1:8080`, CAS in `/var/lib/sconce`, catalog in a
native `services.postgresql`) and a `sconce-ui` container, the operator
dashboard, behind `admin.bougie.tools` (`127.0.0.1:8082`, `ui --single-tenant`
behind HTTP basic auth).

## Component A — sconce image (retired)

> No longer on this box. The hosts had already switched from this Nix-built
> image to the published `ghcr.io/cresset-tools/sconce` image before sconce
> moved to bougierepo.com; see the note at the top of `demo-images.nix`.

Pure-rustls stack (no OpenSSL/libpq/libgit2; only `libc`+`ring` native), so the
image is tiny. Runtime closure = the binary + **`git`** (the in-process mirror
worker shells out to `git clone`) + a **CA bundle** (HTTPS to git/registries) + a
writable `/tmp`. Migrations are embedded in the binary; Postgres is external.

**Toolchain caveat (found in validation).** sconce's crates declare `rust-version =
"1.96"`, but nixpkgs' default rustc is **1.91.1** at our pin (and even current
nixos-unstable is only **1.95.0**) — so a lock bump won't fix it. Pin the toolchain
explicitly from the repo's own `rust-toolchain.toml` via **rust-overlay** (a new
flake input for the demo host) and a `makeRustPlatform`:

```nix
# overlays = [ (import rust-overlay) ];  (flake input: oxalica/rust-overlay)
rustToolchain = pkgs.rust-bin.fromRustupToolchainFile "${sconceSrc}/rust-toolchain.toml";
rustPlatform  = pkgs.makeRustPlatform { cargo = rustToolchain; rustc = rustToolchain; };

sconce = rustPlatform.buildRustPackage {
  pname = "sconce"; version = "0.2.0";
  src = <pinned sconce source>;               # OPEN: how to supply it (below)
  cargoLock.lockFile = "${src}/Cargo.lock";
  cargoBuildFlags = [ "--bin" "sconce" ]; doCheck = false;
};
sconceImage = pkgs.dockerTools.buildLayeredImage {
  name = "sconce"; tag = "demo";
  contents = [ sconce pkgs.gitMinimal pkgs.cacert ];
  config = {
    Entrypoint = [ "${sconce}/bin/sconce" ];
    Cmd = [ "serve" "--cas" "/var/lib/sconce/cas"
            "--listen" "0.0.0.0:8080" "--base-url" "https://repo.bougie.tools" ];
    Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
    Volumes = { "/var/lib/sconce/cas" = { }; };
    ExposedPorts = { "8080/tcp" = { }; };
  };
};
```
Runtime env (via `environmentFiles`): `DATABASE_URL` (native Postgres),
`SCONCE_SECRET_KEY`, plus the mgmt-API service token is issued out-of-band. If we
ever move mirroring to a sidecar, `serve --no-worker` drops the `git` dep from this
image and a separate `sconce worker` container owns it.

**Open — sconce source pinning.** sconce is a private repo; the flake auto-upgrades
from public GitHub, so it can't `fetchFromGitHub` a private repo without a token.
Options: (a) vendor a pinned source copy in-repo like `hosts/telemetry/bougie-collector`;
(b) `fetchFromGitHub` + a deploy-key/read token supplied on the build host;
(c) build the image out-of-band and `docker load` it. Recommend (a) or (b).

## Component B — Magento image (Nix runtime + non-Nix app layer)

**Nix runtime layer — VALIDATED.** `php84.buildEnv` with the store's production
extension set builds against the pinned nixpkgs and loads all extensions
(PHP 8.4.18): `bcmath calendar ctype curl dom exif fileinfo ftp gd gettext gmp
iconv intl mbstring openssl pcntl pdo_mysql redis shmop simplexml soap sockets
sodium sysv{msg,sem,shm} tokenizer xmlwriter xsl zip` + Zend OPcache. (Dev/optional
exts — `xdebug mongodb relay mcrypt yaml ffi` — deliberately dropped.)

```nix
phpRuntime = pkgs.php84.buildEnv {
  extensions = { all, enabled }: enabled ++ (with all; [
    bcmath calendar exif ftp gd gettext gmp intl opcache pcntl
    pdo_mysql redis shmop soap sockets sysvmsg sysvsem sysvshm xsl zip ]);
  extraConfig = "memory_limit=2G\nopcache.enable=1\nopcache.memory_consumption=512\n…";
};
# image = buildLayeredImage { contents = [ (phpRuntime.override…) nginx tini … ];
#   config.Entrypoint = supervisord/s6 launching php-fpm + nginx + `bin/magento cron` }
```

**Non-Nix app layer.** The Magento tree (`app/code` incl. the Cresset modules,
`vendor`, production-compiled `generated`, `pub/static`) is **not** a Nix input.
It's produced once from the proven Phase-2 build:
`composer install --no-dev` → `setup:di:compile` → `setup:static-content:deploy -f`
(production mode) → tar. As built, the seed-tarball idea was superseded: the app
tree is a **Deployer atomic-release tree** under `/var/lib/magento`
(`current -> releases/N`, each release built on-box by `dep deploy` from the
bougie-license-demo repo), with the whole tree bind-mounted into the container so
symlink swaps are honored live. `app/etc/env.php` (DB creds, crypt key, Redis) is
rendered read-only into `shared/` by sops-nix, not baked. This keeps the *runtime*
upgradeable via nixpkgs while the *app* is versioned data.

**Seed to carry** (from Phase 2): the app tree above **plus** a MariaDB dump — and
the dump must include the three modulargento-minimal gaps we added:
`framework-graph-ql`, `module-offline-payments`, `spomky-labs/aes-key-wrap`.

## Component C — datastores (classic NixOS services, native)

All four present & mature at the pin. Gotcha-aware config (the Postgres block
went with sconce on 2026-11-25; the other three remain):

```nix
services.postgresql = {                         # sconce (removed)
  enable = true; package = pkgs.postgresql_17;  # pin; version bumps don't auto-migrate
  ensureDatabases = [ "sconce" ];
  ensureUsers = [{ name = "sconce"; ensureDBOwnership = true; }];  # ensurePermissions removed for pg
};
services.mysql = {                              # Magento — NO services.mariadb
  enable = true; package = pkgs.mariadb;        # REQUIRED (no default) → this makes it MariaDB
  ensureDatabases = [ "bougie_licensing_demo" ];
  ensureUsers = [{ name = "magento"; ensurePermissions = { "bougie_licensing_demo.*" = "ALL PRIVILEGES"; }; }];
};
services.opensearch = {                         # Magento catalog search
  enable = true;
  settings."discovery.type" = "single-node";
  extraJavaOptions = [ "-Xms1g" "-Xmx1g" ];     # heap via OPENSEARCH_JAVA_OPTS (overrides jvm.options); cap ~1g on CX32
  # settings."plugins.security.disabled" = true;  # throwaway box: optional
};
boot.kernel.sysctl."vm.max_map_count" = 262144; # OpenSearch needs a high mmap count
services.redis.servers."".enable = true;        # multi-instance; "" = default 127.0.0.1:6379
```

As built the MariaDB database/user are named `magento` (not
`bougie_licensing_demo`), and its password is applied by a sops-fed oneshot
unit — `ensureUsers.ensurePermissions` doesn't exist at this pin. See
`hosts/demo/configuration.nix`.

## Wiring, secrets, upgrade, reset

- **oci-containers**: `virtualisation.oci-containers.backend = "podman";` one
  container, `magento`, with
  `imageFile = self.packages.x86_64-linux.magentoImage`,
  `extraOptions = [ "--network=host" ]` (reach the loopback datastores),
  a volume for the app tree. (Until 2026-11-25 also `sconce` and `sconce-ui`.)
- **nginx/ACME**: `demo.` → :8081, and `repo.` as a `globalRedirect` to
  `bougierepo.com`; `enableACME = true; forceSSL = true;`, proxy headers via
  `recommendedProxySettings`.
- **Secrets**: **sops-nix** (chosen over hand-provisioned `EnvironmentFile`s —
  DEMO_PLAN decision #2): `modules/secrets.nix` declares them,
  `secrets/demo.yaml` carries them encrypted, and `sops.templates` compose them
  into Magento's `env.php`/`auth.json`. `sconce/service_token` stays: it is the
  store's token for bougierepo.com's licensing API.
- **TLS from PHP**: the image has no `/etc/ssl/certs`, so `phpRuntime` sets
  `curl.cainfo` / `openssl.cafile` to the Nix `cacert` bundle.
- **Upgrade**: bump `flake.lock` → `nix run .#switch -- demo <ip>` rebuilds the
  image (new php/nginx/openssl) + the native datastores; the container restarts;
  rollback via Nix generations. The box also auto-upgrades weekly (Sun 05:00 UTC)
  from `github:cresset-tools/infra#demo`. The app tree upgrades separately (a
  `dep deploy` release), as befits data.
- **Reset (nightly)**: restore the app-tree + MariaDB seed; recreate containers.
  **Not implemented yet** (the open Phase-5 item in DEMO_PLAN.md).

## Validated so far / open items

- ✅ PHP 8.4.18 runtime with the full extension set builds & loads (pinned nixpkgs).
- ✅ Datastores confirmed as classic services at the pin; modular services ruled out.
- ✅ rustc-1.96 gap: nixpkgs ships 1.91/1.95 → pin via **rust-overlay** +
  `fromRustupToolchainFile` (new flake input).
- ✅ **sconce image built & smoke-tested.** `dockerTools.buildLayeredImage` → **59 MiB
  tarball / 167 MB loaded**. `docker run` (native Postgres, `--network=host`): applied
  its 41 embedded migrations (34 tables), logged `serving on …` + `worker ready`,
  ships `git 2.51.2` + CA bundle, no coreutils cruft. Component A is done.
- ✅ **Magento image built & smoke-tested.** `buildLayeredImage` (php84-fpm + all exts
  + nginx + bash/coreutils, entrypoint launches php-fpm+nginx) → **263 MB loaded** (vs a
  typical 1 GB+ Magento image). Proven: (1) `bin/magento --version` in the container →
  **"Mage-OS CLI 3.1.0 (based on Magento 2.4.9)"** — php+exts execute Magento; (2) the
  full serve path works — nginx→php-fpm→Magento **bootstrapped and rendered** (health_check
  returned a Magento **500**, not an nginx 502; the product page rendered a 20 KB dev error
  page). The 500 is only `env.php`'s DB socket path vs the concurrent bougie session's
  shifted live socket — a local env artifact, not the image. A literal 200 comes for free
  on `hosts/demo` with native datastores at fixed paths. Needed a writable `/tmp`
  (`extraCommands = "mkdir -m 1777 tmp"`) and, for production, a real init (s6/tini) so
  php-fpm is ready before nginx accepts (the POC entrypoint has a benign startup race).
- ✅ **sconce source pinning decided.** sconce is **public** (cresset-tools/sconce,
  EUPL-1.2) — pinned by `fetchFromGitHub` rev + hash in `demo-images.nix`. (The private
  repos are the Bougie *modules*, not sconce.)
- ✅ **Secrets mechanism decided: sops-nix.** `.sops.yaml` (three age recipients: two
  admin keys + the demo box's SSH host key), `secrets/demo.yaml` (encrypted, committable),
  `modules/secrets.nix` (declares the five secrets). Decryption at activation with
  `/etc/ssh/ssh_host_ed25519_key` (planted via `nixos-anywhere --extra-files`). Round-trip
  verified. Composed into runtime config via `sops.templates` (sconce env-file + Magento
  `env.php`).
- ✅ **Flake integration complete & evaluated.** Added inputs `rust-overlay` + `sops-nix`
  (both `follows nixpkgs`); `rust-overlay.overlays.default`; `specialArgs = { inherit
  inputs; }`; `packages.x86_64-linux.{sconce,sconceImage,phpRuntime,magentoImage}` from
  `demo-images.nix`. Wrote `hosts/demo/{system,disko.nix,configuration.nix}`. **Verified:**
  `nix eval …demo.config.system.build.toplevel.drvPath` instantiates the full system
  closure; `origin` + `telemetry` still evaluate; both image packages resolve; the two
  DB-password oneshot scripts render correct SQL. (`nix flake check` / full toplevel *build*
  not run — eval is the config check; the image builds are validated separately.)
- ☑ Magento image entrypoint: tini (-g) as PID 1; nginx gated on php-fpm accepting
  on 9000; `wait -n` exits the container if either daemon dies (the podman unit
  restarts it). First-boot seed restore was superseded by the Deployer release
  layout — the app tree lives in `releases/`, seeded by `dep deploy`.
- ☑ Phase 4 shipped and verified live: CX33 provisioned (`nix run .#deploy -- demo <ip>`
  with `--extra-files` planting the SSH host key); DNS + ACME for
  `repo.`/`demo.`/`admin.bougie.tools`; static IPv6 out of the routed /64 with AAAA
  records, reachable externally; the store serving from the Deployer release tree;
  sconce service token provisioned and issuing licenses; Mollie test key re-applied
  under the new crypt key and completing orders end-to-end.
- ☑ **sconce moved to bougierepo.com (2026-11-25).** `cresset` org exported with
  `sconce org-export` and imported with `org-import` (ids and token hashes kept, so
  existing license keys and the service token work unchanged); store switched to
  `https://bougierepo.com`; sconce, sconce-ui, Postgres and `admin.bougie.tools`
  (vhost + DNS) removed from the box; their sops secrets deleted.
