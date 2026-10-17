# Renaming a host

<!-- TODO(ruuda): 1.3. Most of this page is outdated.
 * rename = rename the config dir, `/etc/hostname`, DNS / SSH config, then deploy
 * first deploy after the rename: stale (no tracking ref for the new name), records the host's commit under the new name
 * next deploy: normal; host-side history kept across the rename
 * name swap (web1 <-> web2): same, each host's own history decides
 * refusal "encountered previous deployment as 'web1'": only for a host still on the 1.2 store format, renamed before its first 1.3 deploy
 * that case: rename back and deploy once with 1.3, or delete the store (warning below still applies)
 * "no mechanism to preserve the host-side history" at the end: outdated
 * 1.4: refusal and store deletion gone entirely; a 1.2 host is refused as too old until 1.3 converts it -->

Deptool encourages reprovisioning over renaming hosts. Renames often have
far-reaching unintended consequences. Having said this, even though initial
provisioning is fast with Deptool, reprovisioning is not always feasible,
especially when a host holds state that is difficult to move. This chapter
explains how to handle host renames with Deptool.

## Safety checks

Deptool computes the deployment plan offline, based on the config tree, and
[tracking refs per host](store.md#operator-side-refs). When you rename a host,
the base for this diff is no longer accurate, and the plan will be misleading.
In the best case, the hostname is new, and the renamed host shows up as a new
host in the plan. This is inaccurate, but at least not harmful: deploys are
idempotent. If a hostname is reused though, the base for the diff is incorrect,
and the plan can fail to execute.

To prevent executing incorrect plans against renamed hosts, Deptool prevents
deploying against hosts whose hostname has changed since the previous deploy.
For example, if we deployed against `web1`, and then rename it to `web2` in the
config tree, in its `/etc/hostname`, and in <abbr>DNS</abbr> or <abbr>SSH</abbr>
config, then Deptool would report:

    web2: failed: encountered previous deployment as 'web1'

At this point the host needs manual intervention before it can be deployed to
again. If the rename was unintentional, rename it back. When the rename _was_
intentional, see below.

## Unblocking a renamed host

The way to unblock a renamed host, is to convert it into a fresh install. We can
do that by deleting the Deptool store on the host. Beware of the consequences of
deleting the store!

> **Warning**<br>
> When you delete the Deptool store from a host, Deptool is no longer aware of
> what is deployed there. Although every deployment step is designed to be
> convergent where possible, not everything can be undone without the store.
> For example, when Deptool creates a generic symlink on the filesystem, the
> existence of that symlink is only tracked in the store. Without the store,
> Deptool does not know the symlink exists, and it cannot remove it later.

When you _only_ rename a host, without changing its config tree, deleting the
store and doing a fresh install is safe. Deptool deployments are idempotent, so
deploying the same state is safe.

To proceed, on the renamed host, delete the store that records the deployment
history under the old name:

    rm -fr /var/lib/deptool/store

The next deploy against this host will treat it as a new host, and recreate
the store just as on new installations. Deptool currently has no mechanism to
preserve the host-side deployment history on a renamed host.


