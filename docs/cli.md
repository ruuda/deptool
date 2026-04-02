# CLI Design

## Operator commands

The operator-facing commands are `commit` and `deploy`. These map directly
onto the two things an operator does: record a new desired state, then push
it to the cluster.

`deploy` subsumes planning, confirmation, and applying in one command. There
is no separate `plan` command: the confirmation prompt already shows you what
will change before anything happens, and you can abort there. A `--plan-only`
flag exists for the future use case where one operator computes the plan for
others to review before a separate `deploy` run applies it.

## The `agent` subcommand

The binary that runs on target hosts is called the _agent_, following the
convention established by tools like Puppet and Chef. It is started by the
operator-side `deploy` command over SSH; operators do not invoke it directly.
It lives under `deptool agent` rather than at the top level to make clear
it is not part of the normal operator workflow.

## Store paths

The local store defaults to `./deptool_store`. This is a deliberate relative
path: an operator managing multiple clusters keeps one store per cluster
directory and `cd`s into the right one, the same way you `cd` into a Rust
project before running `cargo`. Deptool does not seek the store upward through
parent directories — explicit is better than implicit, especially for
deployments.

The remote store defaults to `/var/lib/deptool/store`, following the FHS
convention for persistent application state.

Both paths can be overridden with `--store` and `--remote-store`.

## Confirmation UX

The confirmation prompt shows a Git-style diffstat before asking to proceed.
Apps are listed per host with `+`/`~`/`-` prefixes; changed apps list the
affected filenames. Systemd actions use imperative verbs (enable, disable,
restart) because that is what the machine will actually do.

The default answer is abort (uppercase `N`), because a deployment that does
nothing is always safe.

The `d` option opens the full file diff in a pager for each host sequentially.
Per-host rather than per-app because hosts may be at different current
revisions, so a single combined diff across all hosts would be meaningless.
Sequential rather than interactive (e.g. picking a host) to keep the
implementation simple.

## Ideas for future work

 - `deptool status` fetches current `current` refs from target hosts (based on
   the target hosts in the latest commit in the store), and shows what apps are
   currently deployed on them, and what systemd units are enabled. Per app it
   also shows if it's up to date with our latest commit or not. So this is
   similar to a diff but not quite, it shows fewer details about changes, and it
   shows things that are present but unchanged.
 - `deptool agent log`, when run locally on a target machine, shows the
   deployment log, which is just the reflog for the `current` ref. In the reflog
   we should also record the hostname and username of the operator machine that
   initiated the deployment. You want to know who deployed something.
 - `deptool commit` should take a commit message that describes what we are
   deploying. For example, when you generate target configuration with Nix, it
   could mention the store path.
