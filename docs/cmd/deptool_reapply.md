# deptool reapply

    deptool reapply [--no-confirm] [--limit <hosts>]... [--] [<dir>]

## Description

<!-- TODO(ruuda): Describe reapply: converge each host on its deployed commit
to repair drift (re-materialize app checkouts, re-enable and start
manifest-enabled units, without disrupting running units). Mention that
pending config tree changes are deploy's business, and that stale tracking
refs abort the reapply like they abort a deploy. Also mention the two
refusals: a config host without a tracking ref errors with "run 'deptool
sync' first" (another operator may have deployed to it), and a host whose
target ref diverged from current (incomplete deploy) aborts the run;
'deptool deploy' still applies to such a host. -->

## Options

### `--no-confirm`

<!-- TODO(ruuda): Copy the standard --no-confirm boilerplate from
deptool_deploy.md, with "deployment plan" -> "reapply plan". -->

### `--limit <hosts>`

<!-- TODO(ruuda): Copy the standard --limit boilerplate from
deptool_deploy.md, with "deploy" -> "reapply". -->
