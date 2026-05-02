# Store

Deptool stores cluster configuration in a Git repository called the _store_.
It stores all config trees that were ever deployed to the cluster. The store
exists as a bare Git repository on the operator machine, by default in
`.deptool`. It also exists as a bare repository on every target host in
`/var/lib/deptool/store`.

## Data model

A config tree defines the desired state for the entire cluster, as outlined
in the [directory layout](directory_layout.md) chapter. The store stores config
trees as Git trees.

Every deployment is a commit that points to the config tree we deployed or
attempted to deploy. It carries metadata about who created it and when, and it
enables Deptool to not just track what is currently deployed, but everything
that was ever deployed to the cluster.

## Operator-side refs

 * `refs/heads/main` points to the last commit that we attempted to deploy.
 * `refs/remotes/<host>/current` points to the commit that is deployed at that
   host.

On the operator side, Deptool keeps a remote-tracking ref per target host.
This enables Deptool to determine which hosts are affected by a change,
completely offline. For a cluster managed by a single person from a single
store, these refs are always up to date because nothing else mutates the
cluster. In a collaborative setting they may be outdated, just like your Git
remote tracking refs may be outdated when you did not pull for a while. A
`deptool deploy` will discover staleness and update the refs, but if you already
know your local store has outdated refs, you can also run
[`deptool sync`](cmd/deptool_sync.md) to pull the latest state explicitly.

## Target-side refs

 * `refs/heads/current` points to the last successfully deployed commit.
 * `refs/heads/target` points to the target of the last deploy.

On the target machine, Deptool keeps refs to record what is currently deployed
there, and their reflog provides a log of every (attempted) deploy. When the
host is in a clean state, `current` and `target` point to the same commit. At
the start of a deploy, `target` advances to the commit that we attempt to
deploy, while `current` only advances after the deploy is complete. When the two
refs disagree, either a deploy is in progress, or a deploy failed and the target
state is only partially applied.

## Transport

Deptool keeps the Git repositories synchronized between the operator machine and
target hosts by sending packfiles over the existing <abbr>SSH</abbr> agent
session. This skips the additional <abbr>SSH</abbr> handshake that would be
needed for an out-of-band `git push`, and it ensures that Deptool works even
when Git is not installed on the target host. (Deptool embeds libgit2 in its
static binary.)

## Security considerations

TODO(ruuda): document that every host carries a copy of the full cluster store,
including the entire history of every other host's config. Information
disclosure on a compromised host extends to other hosts' configuration, and
secrets that were ever committed remain in the store on every host.

TODO(ruuda): consider restricting host-side stores to only the trees reachable
from that host's own deploys. Trade-off: breaks pack reuse on push.
