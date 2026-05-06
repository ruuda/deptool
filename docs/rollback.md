# Rolling back

Sometimes after you deploy a new configuration, it turns out to be problematic,
and you want to revert to a previous version. In Deptool there are two ways of
doing this:

 * **Automatic rollback during deploy.**
   This type of rollback is performed automatically by Deptool when it detects
   that a deployment failed, and it’s scoped to single hosts.
 * **Reverting a configuration change.**
   Even if a deployment did not fail in a way that Deptool can detect, it might
   still need to be reverted. In this case, you can simply deploy again.

## Automatic rollback during deploy

When a deploy affects an app that contains systemd units, Deptool checks the
status of these units after the deploy. If any unit is in the failed state,
Deptool rolls back to the version previously deployed on this host. This happens
on the host level: if a deploy affects multiple apps, they are all rolled back
on a given host. If the deploy succeeds on some hosts but fails on others, then
it’s only rolled back on the failed hosts, not on the successful ones. If you
run [`deptool diff`](cmd/deptool_diff.md) after that, the rolled-back hosts will
still show up as having pending changes. You can retry the deploy if the problem
was transient, or fix the configuration if need.

See the [deployment phases][deployment] chapter for the full details about how
automatic rollback works, and what its limitations are.

[deployment]: deployment_phases.md#rollback

## Reverting a configuration change

The deployment history that Deptool tracks in its [store](store.md) is
forward-only: it tracks exactly what was ever deployed, new commits only ever
accumulate. To restore a previous configuration, we can deploy the old config
tree again. This is similar to `git revert` for your cluster. We make a _new_
deployment that deploys a config tree that happens to also have been deployed
in the past.

Deptool is intended to be used with a config tree that is _generated_ from a
source of truth kept under source control. For small clusters, if you write
the config tree by hand, you can track the config tree itself in source
control. To revert a deployment, you revert the change in your source-of-truth
repository, and then run [`deptool deploy`][cmd_deploy] again.

[cmd_deploy]: cmd/deptool_deploy.md
