# deptool diff

    deptool diff [--stat] [--limit <hosts>]... [--] [<dir>]

## Description

Show the diff between the config tree in `<dir>`, and the cluster’s currently
known state. This is based on the local tracking refs in the [store](../store.md),
so this command runs entirely offline. Use [`deptool sync`](deptool_sync.md) to
refresh the local view of the cluster when the diff shows unexpected changes.
The diff shown by this command is the same as what `deptool deploy` shows when
you press `d` at the plan confirmation prompt.

See also [`deptool deploy`](deptool_deploy.md) for details about the cluster
config tree directory `<dir>`. As with other commands, diff defaults to the
last-used cluster when you omit it.

## Options

### `--limit <hosts>`

Limit the diff to those listed. Can be provided multiple times, and supports a
comma-separated list of hosts too. For example, in a cluster with hosts `web1`
through `web5`, passing `--limit web1,web2 --limit web3` would exclude `web4`
and `web5` from the diff even if they had changes.

### `--stat`

Pass `--stat` to `git diff` to produce a summarized diffstat instead of a full
line-based diff.

## Environment

### `PAGER`

This command respects the `PAGER` environment variable, and defaults to `less`
when no pager is set.
