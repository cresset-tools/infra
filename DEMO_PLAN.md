# Licensing demo site — plan

A public demo where a visitor "buys" cresset Composer packages (**jibs**,
**magequery**, **wick**, and the **Bougie modules**) with **Mollie in test mode**
and instantly gets a **license key + Composer install instructions** to install
that now access‑gated package from a private **sconce** repo. At least one product
is a **Mollie subscription**, exercising the renewal path in
`cresset/module-bougie-licensing-mollie`.

> Status: **live.** Phases 1–4 are done — the demo serves at
> `demo.bougie.tools` / `repo.bougie.tools` / `admin.bougie.tools` from
> `hosts/demo` (Hetzner CX33), and real test purchases complete end-to-end on
> the live box (paid order → license key issued → gated `composer install`).
> As-built details: `hosts/demo/configuration.nix` +
> [CONTAINERIZATION.md](CONTAINERIZATION.md). Still open from Phase 5:
> nightly reset to a clean seed, and a demo banner / buyer instructions on
> the storefront (the homepage has neither today).

## Locked decisions

- **Hosting:** one always‑on Hetzner host (**x86, 8 GB** — a **CX33** as built)
  added to this flake as `hosts/demo/`, deployed with the existing
  `nix run .#deploy/.#switch`.
- **Magento runtime:** `virtualisation.oci-containers` — as built, Nix-built
  images under rootful podman (no compose), with the datastores as native
  NixOS services; see [CONTAINERIZATION.md](CONTAINERIZATION.md).
- **Store base:** **modulargento minimal**
  (`composer create-project modulargento/project-minimal-edition:3.1.0
  --repository-url=https://modulargento.cresset.tools/` — Mage‑OS 3.1.0, PHP 8.4,
  Luma, public satis/no auth). Lighter footprint: **MariaDB + OpenSearch required;
  Redis optional; RabbitMQ/Varnish not needed.**
- **Package source:** **sconce mirrors every cresset package from git** — no satis
  for the modules. (Validated, see below.)
- **Domains:** `demo.bougie.tools` (store) + `repo.bougie.tools` (sconce) —
  plus `admin.bougie.tools` (sconce operator dashboard, added at go‑live) —
  Cloudflare DNS‑only, ACME like the other hosts.

## Architecture

> Original sketch — superseded in two ways as built: the datastores moved out
> of the compose group to **native NixOS services** (loopback-only), and the
> box is a CX33 with **postgresql 17**. The accurate diagram is in
> [CONTAINERIZATION.md](CONTAINERIZATION.md).

```
        demo.bougie.tools ─┐                    ┌─ repo.bougie.tools
                           ▼                    ▼
  ┌──────────────  hosts/demo  (Hetzner CX32, x86, NixOS) ──────────────┐
  │  nginx + ACME  →  TLS + vhost routing                               │
  │        │  oci-containers                    │  oci-containers        │
  │  ┌── Magento (modulargento minimal) ──┐  ┌── sconce ─────────────┐   │
  │  │ web(nginx+php-fpm 8.4) mariadb:11  │  │ sconce (Rust)         │   │
  │  │ opensearch:2  (redis?)  cron       │◄─┤ postgres:16  + CAS    │   │
  │  └────────────────────────────────────┘  └───────────────────────┘   │
  │  Modules: Cresset_BougieLicensing (+Mollie), mollie/magento2 (+subs)  │
  └────────────────────────────────────────────────────────────────────────┘
     buyer ── composer require cresset/jibs ──► repo.bougie.tools (http-basic: license key)
```

## Component A — sconce (`repo.bougie.tools`)

sconce mirrors each cresset package **from its git repo by tag**, gates it behind
license keys (private visibility), and serves the Composer v2 wire API.

**Package layout (real):** jibs/magequery/wick publish their `cresset/*` Composer
package as **plain `vX.Y.Z` tags on a generated `composer` branch** (root
`composer.json`); the `<tool>-vX` release‑please tags on `main` have no root
manifest and are correctly skipped. The two Bougie modules have a root
`composer.json` on `main` but **no tags yet** — a tag must be cut for sconce to
mirror them.

**Proven Phase‑1 recipe** (local instance in `~/.local/share/sconce-demo/`):

```sh
# instance: fresh DB + secret key + CAS + org/repo
createdb sconce_demo ; export SCONCE_SECRET_KEY=$(openssl rand -base64 32)
sconce org-create cresset --name "Cresset Tools"
sconce repo-create cresset demo

# mirror the three tools (private = license-gated); root composer.json on the
# generated-branch tags is picked up, no --source-path needed
for p in jibs magequery wick; do
  id=$(sconce upstream add --repo cresset/demo --kind git \
         --base https://github.com/cresset-tools/$p.git --visibility private --label $p)
  sconce mirror-upstream "$id" --cas "$CAS"
done

# the two Bougie modules: cut a release tag, then mirror (private repos → add
#   --credential <gh-token> --credential-type github when mirroring the remote)
git -C module-bougie-licensing tag v0.1.0 && git push --tags   # (real hosting: push)
sconce upstream add --repo cresset/demo --kind git \
  --base https://github.com/cresset-tools/module-bougie-licensing.git \
  --visibility private --credential <gh-token> --credential-type github
sconce mirror-upstream <id> --cas "$CAS"

# editions (SKUs): perpetual for tools, one time-bounded to drive renewals
sconce edition create --repo cresset/demo --name Jibs      --slug jibs      --package cresset/jibs      --bound perpetual
sconce edition create --repo cresset/demo --name MageQuery --slug magequery --package cresset/magequery --bound perpetual
sconce edition create --repo cresset/demo --name "Wick — Annual" --slug wick --package cresset/wick     --bound time:12
sconce edition create --repo cresset/demo --name "Bougie Licensing" --slug bougie-licensing --package cresset/module-bougie-licensing --bound perpetual

# a service token for the Magento module (management API), + serve
sconce service-token …            # → scst_… into Magento's Bougie config
sconce serve --cas "$CAS" --base-url https://repo.bougie.tools --listen …
```

Notes:
- **wick was renamed** `cresset-tools/wick` → `cresset/wick` at v0.3.0; sconce
  mirrors both names. Target `cresset/wick` in the edition and floor the legacy
  name out with `--require '*@0.3.0'` on the upstream.
- Private‑repo git auth is `--credential <token> --credential-type github`
  (stored encrypted under `SCONCE_SECRET_KEY`).

## Component B — Magento store (`demo.bougie.tools`)

1. `composer create-project modulargento/project-minimal-edition:3.1.0
   --repository-url=https://modulargento.cresset.tools/`.
2. `composer require cresset/module-bougie-licensing cresset/module-bougie-licensing-mollie
   mollie/magento2 mollie/magento2-subscriptions` → `setup:upgrade` / `di:compile`.
   The Bougie modules are pulled **from `repo.bougie.tools`** (add it as a Composer
   repository with a build credential — the store installs the modules from the
   repo it sells them through).
3. Configure **Bougie** (base `https://repo.bougie.tools`, org/repo `cresset/demo`,
   service token) and **Mollie** (TEST key, enable methods + subscriptions).
4. Catalog: virtual/downloadable products for jibs, magequery, wick, and the
   Bougie modules, each mapped to a sconce edition via the "Bougie edition (SKU)"
   product attribute; **≥1 Mollie subscription** product.

## Reproducibility

Configure the store once locally via the `bougie` toolchain, export a **MariaDB
seed dump + media**, and have the Magento container **restore it on first boot**
(secrets injected via env, not baked). sconce reuses its own `Dockerfile` +
`docker-compose.yml`.

> As built, first-boot restore was superseded by a **Deployer atomic-release
> tree**: the app lives under `/var/lib/magento` (`current -> releases/N`),
> each release built on-box by `dep deploy` from the bougie-license-demo repo,
> with `env.php`/`auth.json` rendered read-only into `shared/` by sops-nix.

## Phases

- **Phase 1 — sconce (DONE, locally proven).** Catalog + editions + gated serving
  + real `composer require`. See Verified.
- **Phase 2 — store locally (via bougie). DONE, locally proven.** minimal + modules,
  wired Bougie + Mollie test, catalog built, full buy→key→install +
  subscription‑renewal flow exercised through real Magento orders. See Verified (Phase 2).
- **Phase 3 — containerize. DONE.** Nix-built OCI images instead of
  Dockerfile/compose — see [CONTAINERIZATION.md](CONTAINERIZATION.md).
- **Phase 4 — host. DONE, live.** `hosts/demo/{configuration,disko}.nix` +
  `system` (x86_64-linux) + sops‑nix (`modules/secrets.nix`); CX33 provisioned,
  DNS + static IPv6, `nix run .#deploy -- demo <ip>`.
- **Phase 5 — go‑live. Partially done.** ACME live; real test purchases proven
  against the live box (orders complete → licenses issued → gated install).
  **Still open:** nightly reset to a clean seed, buyer instructions / demo
  banner on the storefront.

## Verified (Phase 1, 2026‑07‑07)

Against a real local sconce instance:

- **Git mirroring of generated‑branch tags works.** A full `git clone` fetches tags
  that live only on the `composer` branch; sconce reads the **root** `composer.json`
  at each and mirrors it. jibs → `cresset/jibs` v0.2.1/0.3.0/0.4.0 (the `jibs-vX`
  main tags skipped as "unrecognized version"); magequery + wick likewise; both
  Bougie modules mirrored from a local `v0.1.0` tag.
- **Entitlement gate works.** `packages.json` without a key → **401**; with a valid
  key → **200**. A jibs‑only key sees jibs versions `[v0.2.1,v0.3.0,v0.4.0]` but
  `[]` for wick. Dist download: **200** with key, **401** without.
- **Real `composer require cresset/jibs`**: with the key → **exit 0**, installs
  `cresset/jibs v0.4.0`; without the key → **HTTP 401, exit 100 (refused)**.

## Verified (Phase 2, 2026‑07‑08)

Store = modulargento minimal via `bougie`, at `~/bougie-licensing-demo` (tenant DB
`bougie_licensing_demo`). Four virtual products map to editions via the
`bougie_edition` attribute (= edition **slug**; sconce resolves `slug OR name`).
Wick is a Mollie subscription (12 months, infinite). All four flows proven by
driving **real Magento orders** (offline `checkmo`, invoice paid → the
`sales_order_invoice_pay` observer → Provisioner → live sconce):

- **One‑off buy (jibs):** order #1 paid → perpetual license issued, key stored
  (encrypted at rest). **Subscription buy (wick):** order #2 paid → time‑bounded
  license, `bound_until = 2027‑07‑08` (+12 mo), unlinked (initial marker = bare date).
- **Renewal (wick):** order #3 with an ISO‑8601 `subscription_created` marker →
  classified recurring → adopt+link the initial license (`provider=mollie`,
  `sub_id=sub_demoWICK001`) and extend via sconce `/renew`: `bound_until`
  **2027‑07‑08 → 2028‑07‑08**, still **1** wick row (renewed, not duplicated).
- **Install with the buyer's issued key:** decrypt the stored key →
  `composer require cresset/jibs` against `http://127.0.0.1:8080/cresset/demo` →
  **exit 0**, installs `cresset/jibs v0.4.0`; `packages.json` **200** with key,
  **401** without.
- **Mollie test key** decrypts correctly and Mollie's API returns **200** (methods).

**modulargento‑minimal gaps that had to be added** (Phase‑3 seed must include these):
`modulargento/framework-graph-ql:3.1.0`, `modulargento/module-offline-payments:3.1.0`
(no offline method otherwise → scripted/manual orders can't be paid),
`spomky-labs/aes-key-wrap:^7.0` (customer save → JWT token revoke → AES‑KW, else
registration/checkout 500s). All added to `~/bougie-licensing-demo/composer.json`.

**Store gotchas discovered:**
- **Mollie config uses `config_path` overrides** → `bin/magento config:set` rejects
  both the structural and config_path forms. Set `payment/mollie_general/{enabled,
  type,apikey_test}` + `mollie_subscriptions/general/enable` via the config **writer
  + encryptor** in a bootstrap script (how the admin does it). Bougie's own fields
  (`bougie_licensing/general/*`) set fine via `config:set` (obscure `service_token`
  → stored `0:3:…` encrypted).
- **After adding modules, clear `generated/code`+`generated/metadata`** (dev‑mode
  DI is otherwise stale → `Cannot instantiate interface` for Mollie's classes).
- **bougie 0.45 vs 0.46 use version‑keyed service state.** The store's provisioned
  services + tenant DB live under the **0.45** debug build
  (`/home/jelle/bougie/target/debug/bougie`); the installed `bougie` (0.46) sees
  them "not provisioned". Use the **0.45 debug build** for all store ops until the
  store is re‑provisioned under 0.46.

## Open decisions

| # | Decision | Recommendation | Outcome |
|---|----------|----------------|---------|
| 1 | Demo hygiene (public site) | Nightly reset to clean seed; test‑mode Mollie only; "demo" banner | Test‑mode only ✓ (live key empty); nightly reset + banner **still open** |
| 2 | Secrets in infra | Add sops‑nix (new `modules/secrets.nix`) | **Done** — sops‑nix, `secrets/demo.yaml` |
| 3 | Box size | CX32 (8 GB); cap OpenSearch heap ~1 GB | **Settled** — CX33 (8 GB), OpenSearch heap 1 GB |
| 4 | Subscription realism | Trigger a Mollie test recurring charge to show the license bound extend | **Still open** on the live box (proven locally in Phase 2) |

## Prerequisite on the operator

A **Mollie test account + API key**. Everything else is scriptable.
