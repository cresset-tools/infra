# cresset-tools/infra

NixOS configurations for every cresset.tools host, behind one flake.

## Layout

```
flake.nix                        # iterates hosts/*/, exports each as nixosConfigurations.<name>;
                                 # provides .#deploy / .#switch apps that take a host name
hosts/
  origin/                        # Hetzner CAX11 (ARM) + Cloud Volume —
                                 # index.bougie.tools + blobs.bougie.tools
    configuration.nix
    nginx.nix
    disko.nix
modules/                         # shared NixOS modules (empty for now; populates as the
                                 # second host shows up and starts duplicating settings)
```

Add a new host: `mkdir hosts/<name>` + `configuration.nix` + (optional)
`disko.nix`. The flake picks it up automatically — no `flake.nix` edits.

## Hosts

- **`origin`** — serves the bougie distribution layer
  (`index.bougie.tools` + `blobs.bougie.tools`). See
  [`hosts/origin/`](hosts/origin/) and the bootstrap section below.

## Bootstrap a host (one-time)

This walks through `origin`; the same pattern works for any future host
with appropriate substitutions.

### 1. Provision the box + a Cloud Volume

In the Hetzner Cloud console:

1. **Create the server.** Choose **CAX11** (ARM, 2 vCPU, 4 GB RAM, 40 GB
   SSD — the smallest tier, ~€3.79/month) in your preferred location
   (Falkenstein/Helsinki/Ashburn — pick one near your largest user
   base). Image: any current Linux (it gets wiped during install).
   Note the IPv4 address.
2. **Create a Cloud Volume.** Volumes → Create. Start at 10 GB
   (~€0.45/month) — that's plenty for day-zero distribution data, and
   you can resize online up to 10 TB. Attach it to the CAX11 server you
   just created. **Do not** check "Format and mount" — `nixos-anywhere`
   formats it during install per `disko.nix`.
3. **Note the volume ID.** Visible in the Cloud console URL when
   viewing the volume (a numeric ID like `102934857`), or via
   `hcloud volume list`. You'll paste this into `disko.nix` in step 3.

### 2. Point DNS

```
index.bougie.tools  A  <hetzner-ipv4>
blobs.bougie.tools  A  <hetzner-ipv4>
```

ACME's HTTP-01 challenge needs DNS to resolve before NixOS can issue
TLS certs, so do this first and let propagation settle (~5 minutes).

### 3. Edit placeholders

Four TODOs across two files:

**`hosts/origin/disko.nix`** — replace two per-instance IDs:

- `volumeId` — the volume's numeric ID from step 1.
- `bootDiskSerial` — the boot disk's QEMU SCSI serial. With the box
  in rescue mode, run:
  ```sh
  ssh root@<rescue-ip> 'lsblk -o NAME,SIZE,MODEL,SERIAL && ls /dev/disk/by-id/'
  ```
  Look for the QEMU HARDDISK row whose SIZE matches the server's
  primary disk (~40 GB on CAX11). The value you want is everything
  after the `scsi-0QEMU_QEMU_HARDDISK_` prefix in
  `/dev/disk/by-id/`. **This matters**: with a volume attached,
  `/dev/sda` may be the volume rather than the boot disk, so a
  hardcoded `/dev/sda` would clobber the volume during install.

**`hosts/origin/configuration.nix`** — replace:

- `users.users.root.openssh.authorizedKeys.keys` — your laptop's SSH
  public key (for running `nixos-rebuild` against the box).
- `users.users.deploy.openssh.authorizedKeys.keys` — the public half
  of a fresh CI publish key:

  ```sh
  ssh-keygen -t ed25519 -f ~/.ssh/bougie-publish -C 'bougie CI publish'
  ```

  The private half goes into the `cresset-tools/php-build-standalone`
  repo as the `PUBLISH_SSH_KEY` Actions secret. Don't reuse a personal
  key — this is single-purpose by design.

### 4. Boot into Hetzner rescue mode

Hetzner Cloud console → Rescue → enable + power-cycle the server.
You'll get a temporary SSH password shown in the console.

```sh
ssh root@<hetzner-ipv4>
```

### 5. Install with nixos-anywhere

From your laptop, in this repo:

```sh
nix run .#deploy -- origin <hetzner-ipv4>
```

This wraps `nixos-anywhere` with the `origin` host's config:
partitions both the boot disk and the attached volume per `disko.nix`,
installs NixOS, copies the system config, reboots. ~5 minutes on a
fresh CAX11.

> **Cross-arch note.** CAX11 is aarch64; if your operator laptop is
> x86_64 and doesn't have `boot.binfmt.emulatedSystems = [
> "aarch64-linux" ]` configured (most common-case Linux distros don't),
> `nixos-anywhere` will still work because most of nixpkgs has prebuilt
> aarch64 substitutes on cache.nixos.org. If you hit a build that
> needs to compile and your laptop refuses, append `--build-on remote`:
> `nix run .#deploy -- origin <ip> --build-on remote`. Slower per
> compile but reliable, and only matters at install time.

### 6. Verify

```sh
ssh root@<hetzner-ipv4> systemctl status nginx
curl https://index.bougie.tools/index.json
# {"schema":1,"generated":"2024-01-01T00:00:00Z","targets":{}}
```

ACME issuance logs: `journalctl -u acme-index.bougie.tools` and
`acme-blobs.bougie.tools`.

### 7. Wire up CI

In `cresset-tools/php-build-standalone`:

1. **Repo secrets** (Settings → Secrets and variables → Actions):
   - `PUBLISH_SSH_KEY` — the *private* half of the deploy key from
     step 3.
2. **Repo variables**:
   - `INDEX_HOST` = `index.bougie.tools`
   - `BLOB_HOST` = `blobs.bougie.tools`
3. **Edit `.github/workflows/build.yml`** in the `publish` job:
   remove the `if: false  # TODO(hetzner)` line on the rsync step.
   Tag a release; first publish replaces the bootstrapped empty
   index with a real signed one.

## Updating a host after install

```sh
nix run .#switch -- origin <hetzner-ipv4>
```

Wraps `nixos-rebuild switch --target-host`. No SSH-into-the-box
required for ordinary changes.

The `origin` host also auto-upgrades weekly (Sunday 03:30 UTC) from
`github:cresset-tools/infra#origin`, so once you push a change here
the box catches up on its own within a week. For urgent changes use
`nix run .#switch`.

## Operational notes (origin specifically)

### Resizing the volume

When `/srv` gets close to full:

1. Hetzner UI → Volumes → resize to the new size (online, no reboot).
2. SSH to the box and grow the partition + filesystem:
   ```sh
   ssh root@<host> 'growpart /dev/disk/by-id/scsi-0HC_Volume_<id> 1 \
                    && resize2fs /dev/disk/by-id/scsi-0HC_Volume_<id>-part1'
   ```

No nginx restart, no downtime, no data movement. The atomic publish
flip continues unchanged across the resize.

If the volume ever fills up faster than expected, the next escalation
is to detach the volume, attach it to a larger server type, then
detach + reattach to the same CAX11 (or a CAX21/31 if CPU pressure
shows up too). Distribution data stays on the volume regardless.

### What lives where on disk

- Boot disk (`/dev/sda`): NixOS itself, `/nix/store`, journal, ACME
  certs in `/var/lib/acme`. Roughly 4–6 GB used; lots of room.
- Volume (`/dev/disk/by-id/scsi-0HC_Volume_<id>`) → `/srv`: every
  versioned index tree, every blob. This is what fills up over time.

### Other notes

`hosts/origin/` covers nginx vhost layout, /srv structure, atomic
publish flip, and Cloudflare-CDN-when-it-warrants considerations. See
the inline comments in
[`hosts/origin/configuration.nix`](hosts/origin/configuration.nix) and
[`hosts/origin/nginx.nix`](hosts/origin/nginx.nix) for details.
