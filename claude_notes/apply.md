# Applying an App

This document describes how deptool materializes an app on a target host.

## On-disk layout

    /var/lib/deptool/apps/<app>/<oid-prefix>/    checked-out app tree
    /var/lib/deptool/apps/<app>/current          symlink to <oid-prefix>

The oid prefix is the first 10 hex digits of the commit oid. Each checkout
is an immutable directory. We never mutate a checkout in place. Old
checkouts are small (just config files) and are kept indefinitely.

A typical app directory:

    /var/lib/deptool/apps/nginx/current/
    ├── nginx.conf
    ├── mime.types
    ├── manifest.json
    ├── systemd/
    │   └── nginx.service
    └── sysusers/
        └── nginx.conf

Everything the app needs — config files, systemd units, environment files —
lives in one flat (or shallow) directory. A human operator can `ls` and
`readlink current` to understand the state of any app.

## Deploy flow for a changed app

When the plan indicates an app has changed (Add or Update):

 1. Check out the new app tree into `<oid-prefix>/`. If the directory
    already exists (e.g. during a rollback or interrupted deploy), remove
    it first and re-checkout. Config is small so this is fast, and it
    avoids trusting a potentially incomplete checkout.
 2. Atomically swap the `current` symlink: create a temp symlink, then
    `rename(2)` it over `current`.
 3. Systemd phase (see below).

For apps that did not change: do nothing. No checkout, no restart.
Files keep their original mtime/ctime, which aids debugging.

## Systemd units

Deptool makes unit files available to systemd by symlinking them into
`/etc/systemd/system/`:

    /etc/systemd/system/nginx.service -> /var/lib/deptool/apps/nginx/current/systemd/nginx.service

All units under an app's `systemd/` directory are linked, whether or not
they are enabled. Enabling a unit (`systemctl enable`) creates additional
symlinks (in `.wants/`/`.requires/` directories) to activate it on boot.

Unit files reference config through the `current` symlink path:

    [Service]
    BindPaths=/var/lib/deptool/apps/nginx/current/nginx.conf:/etc/nginx/nginx.conf
    EnvironmentFile=/var/lib/deptool/apps/nginx/current/env

Because bind mounts resolve symlinks at mount time, the running service
keeps seeing the old files until it is restarted. The restart is what picks
up the new config.

Deptool identifies its own symlinks in `/etc/systemd/system/` by checking
whether they point into `/var/lib/deptool/`. This makes the operation
convergent: it does not matter what state the system was in before. Crashed
mid-deploy, manually tampered with, fresh boot — the result is the same.

After all per-app checkouts and symlink swaps are done, manifest symlinks
(e.g. config files in `/etc`) are reconciled first -- units may depend on
paths that these symlinks provide. Then the sysusers and systemd phases
run in this order:

 1. Reconcile sysusers symlinks: scan `/etc/sysusers.d/` for symlinks
    pointing into `/var/lib/deptool/`, collect all files across the
    deployed apps' `sysusers/` directories, and create or remove symlinks
    to make them match. If any `sysusers/` content changed in this deploy,
    invoke `systemd-sysusers` to materialize the users. This happens before
    the systemd phase so units can reference users that were just created.
 2. `systemctl disable --now` for units that should no longer be enabled.
 3. Reconcile unit symlinks: scan `/etc/systemd/system/` for symlinks
    pointing into `/var/lib/deptool/` (the actual set), collect all unit
    files across all deployed apps (the desired set), and create or remove
    symlinks to make actual match desired. This runs after disable because
    systemd treats our symlinks as "linked units" and `systemctl disable`
    removes the link itself, not just the enablement symlinks. Reconciling
    here restores them.
 4. `systemctl daemon-reload` to pick up the reconciled state.
 5. `systemctl enable --now` for units that should be newly enabled.
 6. `systemctl restart` for units whose app changed while staying enabled.

## Reboot resilience

Nothing is ephemeral. The `current` symlinks and the `/etc/systemd/system/`
symlinks are persistent filesystem objects. After a reboot, systemd finds
the unit symlinks, resolves them through `current` to the checked-out
version, and starts services normally. No boot-time restore service is
needed.

## Convergence

Applying a commit is idempotent. Regardless of what is currently on disk, the
system will be put in the target state. If the deployment is interrupted
mid-way, for example due to a power outage, the deployment is safe to restart.
An unfinished deployment is also detectable, and due to the use of symlinks per
app, app deployments are atomic (even if the state of the host itself is not),
so even after an interrupted deployment, applications do not observe torn writes
in their configuration.
