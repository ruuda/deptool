# deptool

    deptool [--version] [-h | --help] <command> [<args>]

## Description

The `deptool` executable, see the commands for more details. The commands are:

 * [init](deptool_init.md) — Create an empty store in the current directory.
 * [deploy](deptool_deploy.md) — Deploy a config tree to the cluster.
 * [sync](deptool_sync.md) — Fetch the latest cluster state.

## Global options

### `--store`

Path to the local [store](../store.md), by default `.deptool`.
