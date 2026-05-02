# deptool status

    deptool status [--limit <hosts>]... [--] [<dir>]

## Description

TODO(ruuda): one-line summary, then explain that this command is purely local
(like `git status`), comparing the config tree to the operator-side tracking
refs without contacting any host.

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
