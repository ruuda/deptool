# deptool

    deptool [--version] [-h | --help] <command> [<args>]

## Description

The `deptool` executable, see the commands for more details. The two commands
to get started:

<dl>
 <dt><a href="../deptool_init/"><strong>init</strong></a></dt>
 <dd>Create an empty store in the current directory.</dd>

 <dt><a href="../deptool_deploy/"><strong>deploy</strong></a></dt>
 <dd>Deploy a config tree to the cluster.</dd>
</dl>

Additional commands:

<dl>
 <dt><a href="../deptool_ping/"><strong>ping</strong></a></dt>
 <dd>Measure round-trip latency to each host.</dd>

 <dt><a href="../deptool_sync/"><strong>sync</strong></a></dt>
 <dd>Fetch the latest cluster state.</dd>

 <dt><a href="../deptool_status/"><strong>status</strong></a></dt>
 <dd>Show per-host deployment status, computed offline.</dd>

 <dt><a href="../deptool_diff/"><strong>diff</strong></a></dt>
 <dd>Show the full diff that would be applied by the next deploy.</dd>
</dl>

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

### `DEPTOOL_STORE`

Path to the [store](../store.md), by default `.deptool` in the current
directory.

### `NO_COLOR`

Setting this variable to a non-empty string inhibits colored output, according
to the [`NO_COLOR`][nocolor] standard.

[nocolor]: https://no-color.org/
