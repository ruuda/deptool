# deptool reapply

    deptool reapply [--no-confirm] [--limit <hosts>]... [--] [<dir>]

## Description

Per host, re-apply the revision that is currently deployed there, to repair drift.

In an ideal world, people would never modify files or mutate systemd services
outside of Deptool’s management, and Deptool would manage configuration without
interruptions or bugs. Then there would never be any drift. In reality, Deptool
may have bugs, and people need to address issues on production machines,
sometimes in an ad-hoc manner that creates drift. The `reapply` command aims
to remove such drift.

Reapply proceeds as follows:

 * Like [`deploy`](deptool_deploy.md), it plans and acts based on the current
   local knowledge of the cluster state. If this turns out to be outdated, the
   reapply fails before making changes to any host, and Deptool fetches the
   latest state so that the next run can succeed.
 * If any hosts are defined in the [config tree](../directory_layout.md), but
   the store has no local ref for them (you haven’t deployed or synced these
   hosts yet, at least not from _this_ store), then the reapply aborts, and you
   need to run [`sync`](deptool_sync.md) first. Without this, Deptool would not
   be able to present ahead of time which revision it will reapply at the host.
 * If any host is in a partially applied state (where a deploy failed, but
   could not be rolled back), reapply refuses to act on that host, because it
   is not clear whether to reapply the last successful revision (and therefore
   undo partially applied changes), or the last attempted revision (and
   therefore retry applying changes that are currently not applied).

If the conditions to reapply against the selected hosts are satisfied, reapply
will:

 * Rebuild the `/var/lib/deptool/apps/<app>/<revision>` directory, to remove
   modifications made there outside of Deptool’s management.
 * Reconcile symlinks again, enable systemd units that should be enabled,
   and take the steps following the regular [deployment phases](../deployment_phases.md).
 * Start, but not _restart_ systemd units of enabled units. This means that
   unlike deploy, reapply does not restart systemd units that are already
   running. It does start units that are not running, but should be.

> **Note**<br>
> Reapply does not restart systemd units that are already running. If the
> currently running units are using the wrong configuration (perhaps due to
> earlier manual intervention), then even though Deptool will repair
> `/var/lib/deptool/apps/<app>`, a running service may not pick up that change.
> This is by design: restarting all systemd units in the cluster at the same
> time would likely be disastrous. It does mean, however, that a mere reapply
> may not be sufficient to repair all forms of drift.

## Options

### `--no-confirm`

Proceed without confirmation, [same as for deploy](deptool_deploy.md#-no-confirm).

### `--limit <hosts>`

Limit which hosts to reapply to, [same as for deploy](deptool_deploy.md#-limit-hosts).
