# deptool

    deptool [--version] [-h | --help] <command> [<args>]

## Description

The `deptool` executable, see the commands for more details. The commands are:

 * [init](deptool_init.md) — Create an empty store in the current directory.
 * [deploy](deptool_deploy.md) — Deploy a config tree to the cluster.
 * [sync](deptool_sync.md) — Fetch the latest cluster state.

Deptool respects the [`NO_COLOR`][nocolor] environment variable.

<!-- TODO(ruuda): Document `DEPTOOL_BIN_DIR`. It overrides the directory
deptool searches for cross-arch binaries to push to target hosts (one
subdir per host platform, e.g. `linux-x86_64/`). Defaults to
`$XDG_CACHE_HOME/deptool` or `$HOME/.cache/deptool`. Useful for
local-dev pointing at `target/deptool-bin/`. -->

[nocolor]: https://no-color.org/
