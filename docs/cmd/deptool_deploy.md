# deptool deploy

    deptool deploy [--no-confirm] [--plan-only] [--] [<dir>]

## Description

Read the config tree in directory `<dir>` and deploy it to the cluster defined
in there. This first computes the plan, which happens completely offline. Then
it prompts for confirmation before deploying to the affected hosts. See the
[deployment phases reference](../deployment_phases.md) for details on how the
deployment proceeds.

By convention, the directory `<dir>` is named after the cluster, e.g. `staging`
or `prod`. See the [directory layout reference](../directory_layout.md) for how
`<dir>` should be structured. It is possible for multiple clusters to share a
store, as long as the hosts have no overlap.

When you provide `<dir>` explicitly, Deptool saves it to the `config` file in
the store. On a subsequent run, you can omit `<dir>`, and Deptool will use the
last-used one.

## Options

### `--no-confirm`

By default Deptool prompts for confirmation after presenting the deployment
plan. With this flag, it instead proceeds automatically.

### `--plan-only`

Compute the plan and exit, do not connect to any host.

### `--store`

Path to the local [store](../store.md), by default `.deptool`.
