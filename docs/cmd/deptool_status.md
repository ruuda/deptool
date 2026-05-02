# deptool status

    deptool status [--limit <hosts>]... [--] [<dir>]

## Description

Show the deployment status of every host in the cluster defined in `<dir>`,
based on the local tracking refs in the [store](../store.md). This means that
the status computation is entirely offline. To get the most up to date view of
the cluster, run [`deptool sync`](deptool_sync.md) first.

See also [`deptool deploy`](deptool_deploy.md) for details about the cluster
config tree directory `<dir>`. As with other commands, status defaults to the
last-used cluster when you omit it.

## Output

Example output:

```
a.example.com 2026-04-29 12:32:43 +0200 19bfbda
b.example.com 2026-04-29 12:32:43 +0200 19bfbda undeployed changes: nginx
c.example.com 2026-05-02 14:03:31 +0200 df9b19e
d.example.com 2026-04-29 12:32:43 +0200 19bfbda
e.example.com new host
```

This shows per host:

 * The last time something was deployed there, and the commit hash of the
   deployed version. The hash refers to the commit in the [store](../store.md).
   The time is the time at which the operator initiated the deploy, formatted
   in the original operator’s local time zone.
 * If the config tree in `<dir>` does not match what is deployed on that host,
   a summary of which apps have changes.

Note that different hosts can be at different commits, and still be up to date.
This happens because a commit that modifies only a subset of hosts, does not
need to be deployed to the entire cluster.

## Options

### `--limit <hosts>`

Limit the hosts to show. Can be provided multiple times, and supports a
comma-separated list of hosts too. For example, in a cluster with hosts `web1`
through `web5`, passing `--limit web1,web2 --limit web3` would exclude `web4`
and `web5` from the output.
