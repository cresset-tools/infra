# The canonical monorepo Git remote, self-hosted on `internal`.
#
# Resolved Design Question 2 (design discussion): the canonical monorepo history
# is kept OFF GitHub and self-hosted on the fleet. This module is that remote: a
# bare repository at /srv/git/cresset.git on the host's Hetzner Cloud Volume,
# reachable over SSH as the `git` user for developers and CI
# (`git clone git@internal:cresset.git`).
#
# `internal` is co-located with the existing Authentik + PostgreSQL + cresset-view
# stack (configuration.nix). It carries a dedicated /srv Hetzner Cloud Volume
# (disko.nix), so all worker/canonical-repo state lives under /srv — off the
# root disk and independently sized/backed-up.
#
# Two consumers of the bare repo:
#   - developers / CI, over SSH, push and pull the `origin` remote here;
#   - the cresset-sync worker (cresset-sync.nix), which runs on THIS box and so
#     reads and advances `main` on the bare repo LOCALLY (a guarded expected-head
#     compare-and-swap, no SSH round-trip). See Phase 7's repo.rs advance path.
#
# SSH access for `git` is locked down two ways, belt-and-braces:
#   - the login shell is `git-shell`, which only accepts the git-{receive,upload}
#     -pack / git-shell verbs and refuses interactive logins outright;
#   - every authorized key carries the restrict option set (no pty, no
#     port/agent/X11 forwarding, no user-rc), so a key can do nothing but git.
{ config, pkgs, lib, ... }:
let
  gitHome = "/srv/git";
  bareRepo = "${gitHome}/cresset.git";

  # The handshake between cresset-view and the push gate. cresset-view owns the directory and
  # writes the file; the `git` group reads it from inside the update hook. Deliberately NOT
  # under /var/lib/cresset-view, whose 0750 owner-only directory the hook could not traverse,
  # and deliberately not under /srv/git, which cresset-view has no business writing to.
  approvalsFile = "/var/lib/cresset-review/approved";

  # Present means "let the next push to main through without approvals, then delete me".
  # Under /srv/git because the hook runs as `git` and must be able to remove it; creating it
  # needs host access, which is the point.
  breakGlassFile = "${gitHome}/break-glass";

  # git-shell as the login shell: refuses interactive ssh and only runs the
  # git-shell command whitelist. This is what makes `git@internal` a git-only
  # account even before the per-key restrictions below.
  gitShell = "${pkgs.git}/bin/git-shell";


  # Record every patch set pushed for review, so an amend cannot make the previous one
  # unreachable. See hooks/post-receive for the reasoning; kept as a plain script rather
  # than inlined here so a test can exercise the byte-for-byte file the host installs
  # instead of a copy that drifts from it.
  recordPatchSets = pkgs.writeShellApplication {
    name = "cresset-canonical-post-receive";
    runtimeInputs = [ pkgs.git ];
    text = builtins.readFile ./hooks/post-receive;
  };

  # Guard `main`: fast-forward only, no deletion, and every landing commit approved in
  # cresset-view. See hooks/update — the reasoning is long enough to belong beside the code,
  # and keeping it in a file is what lets the flake check run the bytes the host installs.
  #
  # `logger` for the break-glass audit line; `git` because a hook inherits receive-pack's
  # environment and should not depend on what happens to be on its PATH.
  protectMain = pkgs.writeShellApplication {
    name = "cresset-canonical-update";
    runtimeInputs = [ pkgs.git pkgs.util-linux ];
    text = builtins.readFile ./hooks/update;
  };

  # Belt-and-braces per-key hardening: even though git-shell already blocks
  # everything but push/pull, disable the ssh side channels too.
  restrict = "restrict";

  # Developer / CI deploy keys allowed to push/pull the canonical monorepo.
  # These are the SAME operator keys the rest of the fleet trusts for ops; add
  # per-developer and per-CI keys here as the team grows. Each is prefixed with
  # the `restrict` option so the key can do nothing but talk to git-shell.
  authorizedKeys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMAlEwhbBOJor7VO1Bkv7jLM4aTzElFGSdduEMIz73d7 jelle@dev-debn-02"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICunYiTe1MOJsGC5OBn69bewMBS5bCCE1WayvM4DZLwE jelle@Jelles-MacBook-Pro.local"
    # TODO: add the CI push/pull key(s) that mirror integrated work into the
    # canonical remote (a fresh single-purpose ed25519 key; keep the private
    # half in the CI secret store, not a personal key).
  ];
in
{
  # The `git` service account owns the bare repo tree under /srv. isNormalUser
  # so it gets a real home + can be an ssh login target, but with git-shell as its
  # shell it is git-only. No password (mutableUsers = false, key auth only).
  users.groups.git = { };
  users.users.git = {
    isNormalUser = true;
    group = "git";
    home = gitHome;
    createHome = false; # the init oneshot below owns creation under /srv
    description = "Canonical monorepo git remote (git-shell only)";
    shell = gitShell;
    openssh.authorizedKeys.keys =
      map (k: "${restrict} ${k}") authorizedKeys;
  };

  # git-shell must be a listed login shell or sshd/login refuse it.
  environment.shells = [ gitShell ];

  # Create + initialise the bare canonical repo under /srv/git. /srv is the
  # Hetzner Cloud Volume, mounted with `nofail`, which removes it from
  # local-fs.target's requires set — so, exactly like origin's /srv-dependent
  # services, this must be gated explicitly on the `srv.mount` unit (an
  # after/requires on local-fs.target is NOT enough) before it touches /srv.
  systemd.services.cresset-git-canonical-init = {
    description = "Initialise the canonical bare monorepo at ${bareRepo}";
    wantedBy = [ "multi-user.target" ];
    after = [ "srv.mount" ];
    requires = [ "srv.mount" ];
    # The worker and any push must see the repo present.
    before = [ "cresset-sync.service" ];
    path = [ pkgs.git pkgs.coreutils ];
    # RequiresMountsFor is a [Unit] directive. It sat in serviceConfig until systemd was
    # caught rejecting it -- "Unknown key 'RequiresMountsFor' in section [Service],
    # ignoring" -- so this guard was silently absent on every unit that declared it. The
    # /srv gating held anyway, on `requires = [ "srv.mount" ]`; this restores the second
    # layer it was always meant to have.
    unitConfig.RequiresMountsFor = "/srv";
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      # Belt-and-braces: hold activation until /srv is actually mounted even if
      # ordering is reshuffled.
      # Runs as root to create /srv/git and chown it to `git`; the repo tree
      # ends up entirely git-owned.
    };
    script = ''
      set -euo pipefail

      install -d -o git -g git -m 0755 ${gitHome}

      if [ ! -d ${bareRepo} ]; then
        # --shared=group: the repo is group-writable so the cresset-sync worker
        # (a `git` group member, see cresset-sync.nix) can advance `main` refs
        # LOCALLY, while pushes over SSH still come in as the `git` user.
        git init --bare --shared=group ${bareRepo}
        # `main` is the canonical default branch (matches the monorepo bookmark).
        git -C ${bareRepo} symbolic-ref HEAD refs/heads/main
      fi

      # Installed unconditionally, not only at creation, so a repository made before this
      # protection existed picks it up on the next deploy.
      install -d -o git -g git -m 0755 ${bareRepo}/hooks
      # COPIED, not symlinked. Git treats an unreadable or dangling hook exactly like an absent
      # one — silently, with no error and no rejection — so a link into a store path that ever
      # went missing would disable this protection without anyone noticing. A copy cannot dangle,
      # and re-installing it on every activation keeps it current.
      install -o git -g git -m 0755 ${protectMain}/bin/cresset-canonical-update ${bareRepo}/hooks/update
      install -o git -g git -m 0755 ${recordPatchSets}/bin/cresset-canonical-post-receive ${bareRepo}/hooks/post-receive

      # Where the update hook looks for approvals and for break-glass. In the repository's own
      # config rather than the hook text or the environment: a pusher cannot set these, and the
      # paths stay visible to anyone debugging a refused push with `git config --list`.
      #
      # `--file` rather than `-C ${bareRepo}`: this runs as root against a repository owned by
      # `git`, and git's dubious-ownership check then refuses to discover it — reporting
      # `fatal: not in a git directory`, which reads like the path is wrong rather than like a
      # permissions rule. Writing the config file directly needs no repository discovery.
      git config --file ${bareRepo}/config cresset.approvalsFile ${approvalsFile}
      git config --file ${bareRepo}/config cresset.breakGlassFile ${breakGlassFile}
      git config --file ${bareRepo}/config cresset.reviewUrl https://code.cresset.tools

      # Belt-and-braces: keep the whole tree git-owned across redeploys (a push
      # or the local worker advance may have created objects as `git`).
      chown -R git:git ${gitHome}

      # And keep it group-WRITABLE, which ownership alone does not give. Two accounts
      # write here: `git` for pushes over SSH, `cresset-sync` for local ref advances.
      # A directory created without group write by either one silently breaks the
      # other, and the failure surfaces later and elsewhere — a push rejected with
      # "unable to migrate objects to permanent storage", which points at neither
      # permissions nor the process that caused it.
      #
      # cresset-sync.nix sets UMask=0002 so the WORKER stops causing this. That does not
      # cover an operator: `sudo -u cresset-sync jj ...` in the conflict-resolution
      # workspace inherits the caller's umask, typically 022, and re-creates 2755 object
      # directories that the next push over SSH cannot write. It has happened twice.
      #
      # So run operator commands as that user with `umask 002` set, and treat this repair
      # as the net rather than the plan — it only runs on activation.
      find ${gitHome} -type d -exec chmod g+rwxs {} +
      find ${gitHome} -type f -exec chmod g+r {} +
    '';
  };
}
