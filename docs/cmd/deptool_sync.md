# deptool sync

    deptool sync [--all] [--limit <hosts>]... [--] [<dir>]

## Description

Connect to the hosts defined in the config tree in `<dir>`, and pull their
latest state, so the local store has an up to date view of the cluster.

See also [`deptool deploy`](deptool_deploy.md) for details about the config tree
directory `<dir>`. Just like deploy, sync defaults to the last-used cluster when
you omit it.

Sync is primarily useful in collaborative settings, where you have out of band
knowledge that things were deployed to the cluster from a different store than
your local store, and therefore your [local refs](../store.md#operator-side-refs)
are outdated. If your config tree matches what is deployed (for example because
you generated it from a repository that you just pulled), but your store still
has outdated refs, then [`deptool deploy`](deptool_deploy.md) shows a large plan
that includes changes that are already deployed. When you proceed to deploy,
Deptool discovers that the plan is stale and updates its refs. The next run then
has an up-to-date view of the cluster and shows the expected plan, but still,
the initial plan can look scary. For those situations, `deptool sync` fetches
the cluster state, so you can avoid trying to deploy a stale plan.

## Options

### `--all`

By default Deptool only fetches the state from the hosts that differ between the
config tree in `<dir>`, and the latest known state based on the remote tracking
refs in the store. This is the same set of hosts that `deptool deploy` would
connect to. With `--all`, Deptool instead fetches the latest state from _all_
hosts defined in the config tree.

### `--limit <hosts>`

TODO(ruuda): document `--limit`.

### `--store`

Path to the local [store](../store.md), by default `.deptool`.
