# hosts/bougie-relay — deploy runbook

`bougie-relay` on its own Hetzner CAX11 (ARM). Terminates HTTPS for
`*.bougie.show` and reverse-proxies inbound requests down `bougie share`
tunnels. Its own box on purpose (untrusted public reverse-proxy = its own
blast radius). Config: `configuration.nix`; disk: `disko.nix`.

**Launch posture:** `DEV_ALLOW_ANONYMOUS` — anyone may *view* a share (public
`:443`), but the tunnel-ingress port (`7443`) is firewalled to your IP, so only
you can *create* shares. Swap to sconce introspection once a production sconce
with `/oauth/introspect` exists (drop the anon flag + the tunnel firewall, add
`BOUGIE_RELAY_SCONCE_URL` + `BOUGIE_RELAY_INTROSPECT_SECRET`).

## Prerequisites

- Hetzner Cloud account; `bougie.show` DNS on **Cloudflare** (DNS-only / grey
  cloud, like the rest of the fleet).
- Your `nix`/SSH can read `cresset-tools/bougie-relay` (it's a private flake
  input, fetched over `git+ssh`).
- A `bougie` built from **main** (has `bougie share`, #525). For the Magento
  `base_url` fixup, use a build with **#535** (or main once it merges).

## 1. Provision the box

Hetzner Cloud → new **CAX11** (ARM, ~€4/mo), any image, in a location you like.
Boot it into the **rescue system** (nixos-anywhere installs from there).

## 2. DNS at Cloudflare (grey-cloud / DNS-only)

One wildcard record covers share hosts *and* the tunnel SNI:

    *.bougie.show   A     <box IPv4>          (DNS only)
    *.bougie.show   AAAA  <box IPv6>          (optional, v6 viewers)

(`tunnel.bougie.show` and `<slug>.bougie.show` both match the wildcard, so
that's all you need. DNS-01 validates via TXT, added/removed automatically.)

## 3. Cloudflare API token (for ACME DNS-01)

Create a token scoped to **Zone → DNS → Edit** on the `bougie.show` zone. You'll
place it on the box in step 6 (it can't be committed — this repo auto-upgrades
from a public GitHub repo).

## 4. Fill the placeholders

In `disko.nix`: `bootDiskSerial` (from the rescue shell:
`ls /dev/disk/by-id/ | grep QEMU_HARDDISK`).

In `configuration.nix`:
- `interfaces.enp1s0.ipv6.addresses` → the box's routed `/64` (Hetzner console →
  Networking), as `<prefix>::1`.
- `tunnelAllowFrom` → your current IPv4 (`curl -s ifconfig.me` on the Mac).

## 5. Install

From this repo on your Mac:

    nix run .#deploy -- bougie-relay <box-ip>

(nixos-anywhere partitions per `disko.nix`, installs, reboots. It builds the
relay from the private input using your SSH access.)

Later updates:

    nix run .#switch -- bougie-relay <box-ip>

## 6. Place the Cloudflare token, then let ACME issue

On first boot `acme-bougie.show.service` fails (no token yet) and the relay
stays down — fail-closed. SSH in and:

    install -d -m 700 /var/lib/acme-secrets
    printf 'CF_DNS_API_TOKEN=%s\n' '<token>' > /var/lib/acme-secrets/cloudflare.env
    chmod 600 /var/lib/acme-secrets/cloudflare.env
    systemctl start acme-bougie.show.service     # issues the wildcard via DNS-01
    systemctl start bougie-relay.service

## 7. Verify

    systemctl status bougie-relay acme-bougie.show
    ls /var/lib/acme/bougie.show/                 # fullchain.pem, key.pem
    curl -sS https://anything.bougie.show/ -o /dev/null -w '%{http_code}\n'
      # relay reachable + real cert → expect its "no such share" page, not a TLS error

## 8. Share against it

From a project on your Mac (bougie ≥ main; #535 for Magento URLs):

    BOUGIE_SHARE_RELAY=tunnel.bougie.show:7443 \
    BOUGIE_SHARE_RELAY_SNI=tunnel.bougie.show \
    bougie share

(No `BOUGIE_TUNNEL_CA` needed — the relay presents a real Let's Encrypt cert
the client already trusts. The tunnel is on `7443`, firewalled to your IP.)

Open the printed `https://<slug>.bougie.show/` and your `*.bougie.run` loopback
side by side — both should work, assets differing per host.

## Follow-ups

- **autoUpgrade is off**: the relay is a private input, so the box needs its own
  read token for `cresset-tools/bougie-relay` before the Sunday auto-upgrade can
  fetch it. Until then, update by hand (`nix run .#switch`).
- **Dynamic IP**: `tunnelAllowFrom` is a single IP; re-`switch` when yours moves
  (or move to a shared-secret tunnel gate / sconce auth).
- **Tunnel on 7443, not 443**: fine for your own network; revisit if a
  restrictive network blocks outbound 7443 (SNI-mux onto 443, or a second IP).
- **sconce auth**: the real end state — deploy a production sconce, then flip
  the relay off anonymous.
