# deptool

    deptool [--version] [-h | --help] <command> [<args>]

## Description

The `deptool` executable, see the commands for more details. The commands are:

 * [init](deptool_init.md) — Create an empty store in the current directory.
 * [deploy](deptool_deploy.md) — Deploy a config tree to the cluster.
 * [sync](deptool_sync.md) — Fetch the latest cluster state.
 * [ping](deptool_ping.md) — Measure round-trip latency to each host.

## Environment

The following environment variables affect Deptool’s behavior:

### `DEPTOOL_BIN_DIR`

For cross-platform deploys, where the target host is a different platform than
the operator machine (for example, deploying against a _Linux x86_64_ host from
a _OpenBSD arm64_ host), Deptool needs a `deptool` binary for the target
platform. It looks for those binaries in `DEPTOOL_BIN_DIR`, in subdirectories
named after the target platform (`uname -sm` output lowercased and spaces
replaced with dashes, e.g. `linux-x86_64`). When this variable is not set,
Deptool falls back to:

 * `$XDG_CACHE_HOME/deptool`, if `XDG_CACHE_HOME` is set.
 * `$HOME/.cache/deptool`

### `NO_COLOR`

Setting this variable to a non-empty string inhibits colored output, according
to the [`NO_COLOR`][nocolor] standard.

[nocolor]: https://no-color.org/
