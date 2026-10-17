# Deployment phases

A `deptool deploy` run goes through the following phases to apply the config
tree to the cluster.

## Plan

Based on the config tree to deploy, and host tracking refs in the store, Deptool
compares the desired cluster state against the currently known state. This
informs which hosts are affected, and what the changes are on that host. It
displays this as the _plan_, and asks the operator to confirm before proceeding.

<!-- TODO(ruuda): 1.3, plan phase.
 * creates no commits
 * deploy commits made after confirmation (`--no-confirm`: right away), before connecting -->

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

<!-- TODO(ruuda): 1.3, store conversion at lock.
 * host store on version 1 (1.2 or older): agent converts after taking the lock
 * `current` (and a diverged `target`) get a new per-host commit, parent = the old cluster commit
 * reflog: "convert to per-host format"
 * that deploy then aborts as stale; the operator fetches the converted commit
 * re-run `deptool deploy`: a normal deploy; one stale round per host
 * renamed before conversion: refused, see renaming_hosts.md -->

Locks are implemented as a file lock held by the agent processes on the target
host. Deptool spawns the agent process through an <abbr>SSH</abbr> session.
When `deptool deploy` exits, or when you kill it, the <abbr>SSH</abbr>
subprocesses terminate, which in turn stops the agent on the target hosts, and
this releases the locks. You never need to break a lock manually.

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

### Update networkd config

Reconcile Deptool symlinks in `/etc/systemd/network` to match the current
[networkd](directory_layout.md#networkd) files defined across the host’s apps.
If the contents of any of the `networkd` directories changed, execute
`networkctl reload`. Deptool forwards the output of `networkctl status` to the
operator side, so the operator can see what the effect was.

### Update quadlets

Reconcile Deptool symlinks in `/etc/containers/systemd` to match the current
[quadlet](directory_layout.md#quadlets) files defined across the host’s apps.

### Update sysusers

Reconcile Deptool symlinks in `/etc/sysusers.d/` to match the current
[sysusers](directory_layout.md#sysusers) defined across the host’s apps. If the
contents of any of the `sysusers` directories changed, execute
`systemd-sysusers` to ensure that users exist before the next step, so that
systemd units can reference the users we just created. Deptool forwards the
output of `systemd-sysusers` to the operator side, so the operator can see what
the effect was.

### Stop and disable systemd units

For any systemd units that were enabled in the previous revision, but not in the
one we are deploying, run `systemctl disable --now` on them.

Note, the systemd steps only execute when an app includes systemd units. Deptool
works fine on non-systemd systems, though at this time it has no special support
for other service managers.

### Update systemd units

Reconcile Deptool symlinks in `/etc/systemd/system` to match the current
[systemd units](directory_layout.md#systemd) defined across the host’s apps.

For all units that are enabled in the current revision, run `systemctl enable
--no-reload` on them. The enable is needed even for units that were already
enabled in the previous revision, because systemd resolves the
`/var/lib/deptool/apps/<app>/current` symlink to an absolute path upon enable.
Without the re-enable, systemd would keep references to app directories for
revisions that have since been garbage collected.

Next, issue a `systemctl daemon-reload` to pick up changes to units, including
changes to `.wants` symlinks created by enabling units.

### Restart systemd units

For enabled systemd units that are part of an app that was changed in any way,
start or restart those units with `systemctl restart`. (Restart starts units
that were not already started.) This ensures that we never forget to pick up new
configuration, but it may have false positives: cases where we restart the
service even when it was not needed. For example, when we created a new symlink
that does not affect the service. Although some services support reloading their
configuration, for simplicity we always restart.

Deptool restarts all affected systemd units in parallel.

In case of a [reapply](cmd/deptool_reapply.md) rather than a
[deploy](cmd/deptool_deploy.md), Deptool runs `systemctl start` rather than
`restart`. Start merely starts units that are not already running, it does not
restart running units. Restarting _all_ managed units across the _entire_
cluster at the exact same time would be a very bad idea!

### Check systemd unit status

Deptool waits briefly to confirm that units enabled in the manifest started
successfully. This ensures that we report a useful status back to the operator.
(A daemon might start, but crash after 5ms because it couldn’t read a config
file, or bind to a port.) It also enables Deptool to detect more reliably when
a deployment failed, and start a rollback.

 1. We wait for systemd to signal that it started affected units, by waiting
    for the `systemctl restart` command to exit. This command blocks until the
    current instance has exited, which can take a while in case of a graceful
    shutdown, for example when a webserver waits until all connections are
    closed.
 2. After a unit first becomes `active`, we continue to poll its status for
    500ms to confirm that it stays up. A one-shot unit that finishes
    successfully and ends up in the `inactive` state is also considered a
    success.
 3. For units that remain `starting`, we poll up to 10 seconds. After that
    deadline, Deptool assumes that the deploy is successful.

If a unit fails at any point, Deptool considers the deploy failed. If the
deployment is rollback-safe, Deptool initiates the rollback at the first
failure; it does not wait for the status of other units. If all units that
are enabled stay up, apply is complete on this host.

> **Tip**<br>
> A `Type=simple` systemd unit does not provide a reliable way to measure that
> startup was successful and complete. When the application supports it,
> `Type=notify` is more precise: it enables Deptool to wait until the daemon
> itself confirms that it started successfully. For applications that don’t
> support `notify`, consider using `Type=exec`, which waits for the `execve`
> call to succeed before marking the unit as `active`. This matters especially
> for units that use mounts: `Type=simple` reports a unit as `active` when
> systemd is still setting up prerequisites, which might fail. See also the
> [systemd `Type=` documentation][sd-type].

[sd-type]: https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html#Type=

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
