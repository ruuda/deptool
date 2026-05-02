# deptool sync

    deptool sync [--changed] [--limit <hosts>]... [--] [<dir>]

## Description

Connect to the hosts defined in the config tree in `<dir>`, and pull their
latest state, so the local store has an up to date view of the cluster.

See also [`deptool deploy`](deptool_deploy.md) for details about the cluster
config tree directory `<dir>`. As with other commands, sync defaults to the
last-used cluster when you omit it.

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

### `--changed`

By default, `deptool sync` syncs all hosts, which for large clusters can
be wasteful. `--changed` limits this to only the hosts whose last know state
differs from the config tree in `<dir>`. In other words, `deptool sync --changed`
syncs only those hosts that `deptool deploy` would connect to.

### `--limit <hosts>`

Limit the hosts to sync to just those listed. Can be provided multiple times,
and supports a comma-separated list of hosts too. For example, in a cluster with
hosts `web1` through `web5`, passing `--limit web1,web2 --limit web3` would
exclude `web4` and `web5` from the sync.

