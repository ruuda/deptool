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

## Deployment workflow

When we run `deptool deploy`, Deptool first proceeds to make a _plan_.

 * Read the `main` ref, this is the target we want to deploy.
 * For every host in the tree we want to deploy, check if we have a remote ref
   for that host. If we do, compare the tree for its `current` under its own
   hostname against our target tree. If there are no changes, we don't even need
   to SSH into this host.
 * If we have no remote ref, or if we have but it's different from our target
   tree, then execute a git fetch against the host to obtain its latest
   `current` and `target` ref. If this changed the situation and it turns out
   the host is already up to date, great, again nothing to do.
 * If the tree is still different then we will need to execute something on this
   host. Confirm that the current ref on the host is an ancestor of our target,
   if not then there is some divergence and we need to abort. Then diff the
   current and target trees (but only one level deep), this tells us what is
   changing per profile. A profile could be added to this host, removed from it,
   or it could be updated to a new version.
 * This per-host diff of profiles is the plan, which we display to the user and
   ask to confirm. If needed, the user can view the diff for individual profiles
   and see exactly how every file will change.

When the plan is approved, we apply it:

 * Git push new commits to hosts that need changing.
 * SSH into the hosts that need changing, start the client part of the binary
   with target commit, which will then do the following:
 * Move the `target` ref to our target.
 * Check out the modified profiles into new directories. To be specifed in more
   detail later.
 * Apply this, which will involve something with mounts and restarting systemd
   services, also to be fleshed out later. Record the output of `systemctl
   status` for reporting back to the operator.
 * This machine is done, move the `current` ref to the same commit as `target`.

## Unresolved questions

 * Removing _all_ profiles from a host is currently not something we can do
   because you can't have empty directories in Git trees. Maybe we can just
   leave a file `EMPTY` in the tree for that host. Or maybe we should have a
   file `META` in every host besides its profile to clarify inventory? Or maybe
   we should have a top-level inventory file. But `META` per dir sounds kinda
   nice, I think, in case we need to track more in the future.
