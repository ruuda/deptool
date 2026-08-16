# deptool init

    deptool init [<dir>]

## Description

Initialize a new empty [store](../store.md). By default the store is
located at `.deptool` in the current directory. You can override this
with [`DEPTOOL_STORE`](deptool.md#deptool_store).

<!-- TODO(ruuda): Document that re-running init on an existing store is safe:
it leaves the store untouched and reports "Using existing store at ...". -->

If a cluster directory `<dir>` is provided, create that directory, and record it
as the default cluster directory. A single store can serve multiple clusters, as
long as there is no overlap between the hosts.
