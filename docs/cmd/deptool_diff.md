# deptool diff

    deptool diff [--stat | --word-diff | --word-diff-regex <regex>]
                 [--limit <hosts>]... [--] [<dir>]

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

### `--word-diff`

Pass `--word-diff` to `git diff`, which shows changed words within a line,
rather than marking an entire line as removed and added. By default, Git splits
lines on whitespace, but this can be changed with
[`--word-diff-regex`](#-word-diff-regex-regex).
See the [Git manual][word-diff] for details.

[word-diff]: https://git-scm.com/docs/diff-options#Documentation/diff-options.txt---word-diffmode

### `--word-diff-regex <regex>`

Pass `--word-diff-regex` to `git diff`. Git then uses `<regex>` to decide what
a word is, instead of considering runs of non-whitespace to be a word. See the
[Git manual][word-diff-regex] for details.

[word-diff-regex]: https://git-scm.com/docs/diff-options#Documentation/diff-options.txt---word-diff-regexregex

## Environment

### `PAGER`

This command respects the `PAGER` environment variable, and defaults to `less`
when no pager is set.
