# Deployment workflow

## Planning

When we run `deptool deploy`, Deptool first proceeds to make a _plan_.

 * Read the `main` ref, this is the target we want to deploy.
 * For every host in the tree, check if we have a local tracking ref
   `refs/remotes/<host>/current`. If we do, compare the tree under that host's
   name against our target tree. If there are no changes, this host is already
   up to date and we skip it.
 * If we have no tracking ref, or if the trees differ, diff the current and
   target trees (but only one level deep). This tells us what is changing per
   app. An app could be added to this host, removed from it, or updated.
 * This per-host diff of apps is the plan, which we display to the user and ask
   to confirm. If needed, the user can view the diff for individual apps and see
   exactly how every file will change.

The plan is based entirely on local state. Tracking refs are updated eagerly
during the locking and applying phases, so the plan always has reasonably fresh
data.

## Locking

When the plan is approved, we lock all hosts before making any changes.

 * For each host in the plan (in asciibetical order, to prevent deadlock), open
   an agent session over SSH and send a `Lock` request. The lock includes the
   commit we expect the host's `current` ref to point to.
 * The agent acquires an exclusive file lock (`flock`) on a lockfile in the
   store. Then it compares its `current` ref to what the operator expected.
 * If it matches: respond `Locked`. The agent holds the flock for the lifetime
   of the session process.
 * If it doesn't match: respond `LockStale` with the actual commit. The
   operator fetches the actual commit (so the next plan has fresh data) but does
   not deploy to this host.
 * If the flock is already held: respond `LockBusy`, another deploy is in
   progress.
 * We try _all_ hosts, even if some fail. This way a single run gathers all
   stale info. If any host failed to lock, we abort the deploy entirely (nothing
   was changed).

## Pushing objects

All hosts are locked. Now the operator sends the Git objects needed for the new
commit to each host. See [transport.md](transport.md) for how this works. In
short: we build a packfile with libgit2 and send it base64-encoded over the
session. No second SSH connection, no Git required on the target.

## Applying

For each locked host:

 * Send an `Apply` request with the target commit.
 * The agent sets `refs/heads/target` to the target commit.
 * Check out changed apps into new directories.
 * Reconcile systemd unit symlinks, then `daemon-reload` and
   `enable`/`disable`/`restart` as needed.
 * Set `refs/heads/current` to the target commit.
 * Respond with `ApplyComplete`.
 * The operator immediately updates its local tracking ref
   `refs/remotes/<host>/current`. This happens per-host as each completes, so
   progress is not lost if a later host fails.
