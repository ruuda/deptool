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

TODO(ruuda): show example output for the three states (`new host`, `up to
date`, `undeployed changes in ...`) and explain the timestamp format
(`YYYY-MM-DD HH:MM:SS ±HHMM`, matches `git log %ci`, in the original commit zone).

## Options

### `--limit <hosts>`

Limit the hosts to show. Can be provided multiple times, and supports a
comma-separated list of hosts too. For example, in a cluster with hosts `web1`
through `web5`, passing `--limit web1,web2 --limit web3` would exclude `web4`
and `web5` from the output.
