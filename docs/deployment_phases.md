# Deployment phases

A `deptool deploy` run goes through the following phases to apply the config
tree to the cluster.

## Plan

Based on the config tree to deploy, and host tracking refs in the store, Deptool
compares the desired cluster state against the currently known state. This
informs which hosts are affected, and what the changes are on that host. It
displays this as the _plan_, and asks the operator to confirm before proceeding.

## Connect and lock

If the operator approves the plan, Deptool connects to all affected hosts in
parallel. If needed, it copies the agent binary to the host. Next it tries to
obtain the deploy lock on each host, which prevents concurrent deploys to the
same host. Deptool never waits to acquire the lock — if the lock is already
held, deploy aborts, and the operator can retry `deptool deploy` later.
Because there is no waiting, deadlocks are not possible.

Obtaining the lock fails when the local view of the cluster was outdated.
I.e. something was deployed against that host, but from a different Deptool
store. This can happen when collaboratively managing a cluster. If the local
view was outdated, then the plan is stale, and the deploy aborts. Deptool
fetches any missing commits so the next `deptool deploy` run has an up-to-date
view of the cluster.

## Apply

When Deptool holds the deploy lock for every affected host, it proceeds to the
_apply_ phase. In parallel, every host independently goes through the steps
below, in this order. The steps are per host, not per app.

### Checkout

For every affected app, create a new directory `/var/lib/deptool/apps/<app>/<hash>`
named after the commit we are deploying. Then point the app’s `current` symlink
to it, and point `previous` at what `current` used to point to. This ensures
configuration files change atomically per app.

### Update symlinks

If there were changes to [symlinks defined in the manifest](manifests.md#symlinks),
apply those to the filesystem. Symlinks point through the app’s `current`
symlink, so this is only needed if symlinks change, not if the contents of the
target change.

### Update sysusers

Reconcile Deptool symlinks in `/etc/sysusers.d/` to match the current
[sysusers](directory_layout.md#sysusers) defined across the host’s apps. If the
contents of any of the `sysusers` directories changed, execute
`systemd-sysusers` to ensure that users exist before the next step, so that
systemd units can reference the users we just created.

### Stop and disable systemd units

For any systemd units that were enabled in the previous revision, but not in the
one we are deploying, run `systemctl disable --now` on them.

### Update systemd units

Reconcile Deptool symlinks in `/etc/systemd/system` to match the current
[systemd units](directory_layout.md#systemd) defined across the host’s apps.
Then run `systemctl daemon-reload` to pick up changes to units.

Note, the systemd steps only execute when an app includes systemd units. Deptool
works fine on non-systemd systems, though at this time it has no special support
for other service managers.

### Enable and start systemd units

For any systemd units that were not enabled in the previous revision, but which
are enabled in the one we are deploying, run `systemctl enable --now` on them.

### Restart systemd units

For systemd units that are part of an app that was changed in any way,
restart those units with `systemctl restart`. This ensures that we never forget
to pick up new configuration, but it may have false positives: cases where we
restart the service even when it was not needed. For example, when we created
a new symlink that does not affect the service. Although some services support
reloading their configuration, for simplicity we always restart.

### Check systemd unit status

We wait 300ms to give services the opportunity to start. Then for all apps that
changed, we check the `systemctl status` of the units that are enabled in their
manifest. If none of them failed, apply is complete on this host.

## Rollback

If in the _check systemd unit status_ step any unit failed, we roll back if
possible. Rollback is the same as the _apply_ phase, but this time we deploy the
previous, known-good commit. Rollback happens per host: if a deploy succeeded on
host <abbr>A</abbr> and failed on host <abbr>B</abbr>, then the deploy on host
<abbr>A</abbr> will not be rolled back, but the deploy on host <abbr>B</abbr> will.

Rollback ensures that critical services continue running. For example, if a
webserver fails to start after a configuration change — perhaps because the new
configuration contains a syntax error — then instead of leaving the unit in the
failed state and incurring downtime, we start the webserver again with its
previous, known-good configuration. The fact that the previous version could be
started at the time is no guarantee that the service can still start this time
(the failure may have an external cause), but overall rollback is more useful
than not trying at all.

Rollback is not possible for plans that create new symlinks on the target host.
The Deptool-managed symlink might overwrite files that were previously present
on the system, and Deptool can’t restore the unmanaged contents later. For this
reason, it’s best to separate changes that create new things from changes that
merely change or remove apps, such that at least those latter changes can
benefit from rollback. Deptool prints whether rollback is available ahead of
time, when it displays the plan.
