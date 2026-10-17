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

<!-- TODO(ruuda): 1.3, per-host history. Paragraph above is outdated.
 * one commit per host per deploy, no cluster-wide commit
 * tree: that host's apps only (no host directory level)
 * parent: that host's previous commit; linear history per host
 * "everything ever deployed to the cluster": union of per-host histories
 * message: subject for this host; body lists every host and app of the run
 * `Deploy-Run: <token>` trailer shared by one run's commits; `git log --all --grep`
 * commits created only when a deploy proceeds; not for diff, plan-only, abort -->

## Operator-side refs

 * `refs/heads/main` points to the last commit that we attempted to deploy.
 * `refs/remotes/<host>` points to the commit that is deployed at that
   host.

<!-- TODO(ruuda): 1.3. The `main` bullet above is outdated.
 * `refs/heads/main` no longer written or read
 * an existing `main` in an older store stays, unused
 * `refs/remotes/<host>` now points to that host's own commit -->

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

<!-- TODO(ruuda): Store format version.
 * host store Git config: `deptool.storeVersion`
 * written by the agent when it takes the deploy lock
 * unsupported (newer) version: deploy refused, "store format version N is too new"; fix: update Deptool
 * version 1: cluster tree per commit (1.2)
 * version 2: per-host commits (1.3)
 * no version recorded (1.1 and older): treated as version 1
 * 1 -> 2: agent converts `current` and `target` at lock; see deployment_phases.md -->

## Transport

Deptool keeps the Git repositories synchronized between the operator machine and
target hosts by sending packfiles over the existing <abbr>SSH</abbr> agent
session. This skips the additional <abbr>SSH</abbr> handshake that would be
needed for an out-of-band `git push`, and it ensures that Deptool works even
when Git is not installed on the target host. (Deptool embeds libgit2 in its
static binary.)

## Security considerations

The store holds the configuration for the entire cluster, and its full history.
Currently every managed host contains a copy of the store. This means that every
host stores not just its own current configuration, but also configuration for
other hosts. Although you should never store secrets in your configuration, and
your cluster should be secure even when its configuration is public, this still
presents an information disclosure risk. To mitigate this somewhat, the store on
managed hosts is accessible only to the `root` user.

<!-- TODO(ruuda): 1.3. Paragraph above is outdated.
 * a host receives only its own commits, so only its own config
 * except the run body: lists other hosts' names and app names (not their files)
 * a converted host keeps the cluster-wide history it already had
 * to drop that: delete the host store (see renaming_hosts.md for the consequences) -->
