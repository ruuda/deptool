# Store

Deptool stores state in Git repositories.

 * Cluster configuration as Git trees.
 * The history of cluster configuration as a Git branch.

## Target machine

On a target machine inside a cluster, we have two refs:

 * `refs/heads/current` for what is currently checked out.
 * `refs/heads/target` for what we intend to check out.

Usually these two refs point to the same commit, but we split them such that if
something fails during a deployment, we know it was not finished and we can
recover.

The reflog for these refs gives us the deployment history for free. It is kept
locally on the machine so it is the source of truth.

## Developer machine

On the developer machine, from which the operator runs `deptool`, we mirror the
refs of the target machines using the standard Git remote conventions.

 * `refs/remotes/<hostname>/current` is what is checked out at the host.
 * `refs/remotes/<hostname>/target` is the target for that machine.

Locally on the developer machine we furthermore have:

  * `refs/heads/main`, the standard main branch, which is where we add new
    commits when we `deptool commit`.
  * `refs/heads/current`, the last thing that we know we deployed against the
    cluster.

The deployment workflow is documented in [deployment.md](deployment.md).

## Unresolved questions

 * Removing _all_ profiles from a host is currently not something we can do
   because you can't have empty directories in Git trees. Maybe we can just
   leave a file `EMPTY` in the tree for that host. Or maybe we should have a
   file `META` in every host besides its profile to clarify inventory? Or maybe
   we should have a top-level inventory file. But `META` per dir sounds kinda
   nice, I think, in case we need to track more in the future.
