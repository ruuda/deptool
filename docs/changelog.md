# Changelog

## Versioning policy

Deptool versions are named `MAJOR.MINOR.PATCH`.

 * The major version number is purely cosmetic and represents the author’s
   sentiment.
 * The minor version is bumped for new features and changes that are not bugfixes.
 * The patch version is bumped for bugfixes.

The version number is **not** a [semantic version][semver]. Changes that have
compatibility impact will be clearly marked as such in the changelog.

[semver]: https://semver.org/

## Next

Unreleased.

<!-- TODO(ruuda): Deploys now refuse hosts whose deployed commit has no config
entry for their hostname (the state a hostname change leaves behind), instead
of looping on 'stale' forever. Reference the new 'Renaming a host' page. -->

<!-- TODO(ruuda): The agent now records the hostname in the host store on every
apply, and the lock check compares it against /etc/hostname. This makes rename
detection exact (it also catches name swaps and reused names, which the
tree-entry check missed); the error names the old hostname. Hosts deployed
before this release use the tree-entry check until their next apply records
the name. -->

New features:

 * Add support for [Podman quadlets](directory_layout.md#quadlets).
 * Add support for [systemd-networkd config](directory_layout.md#networkd).
   Deptool executes `networkctl reload` after a managed networkd file changes.
 * Add [`deptool reapply`](cmd/deptool_reapply.md), a command that re-applies
   the currently deployed revision on every host in the cluster. This command
   should not be needed normally, but it helps to recover from manually created
   drift and bugs.

Improvements:

 <!-- TODO(ruuda): Passing a config tree directory that does not exist now
 fails with an error that includes the path, instead of a bare io error, and
 no longer records the bad path as the default for subsequent runs. -->
 * Do not depend on shell brace expansion when installing the agent to improve
   compatibility, and print stderr when agent installation fails to aid
   debugging.
 <!-- TODO(ruuda): The install-failure stderr is now captured at every step
 of the session (probing, sending the binary, reading the checksum), not
 only the missing-checksum case. Could fold into the bullet above. -->
 <!-- TODO(ruuda): Agent lock errors now include the lock file path ("failed
 to open lock file '<path>'", "failed to lock '<path>'") instead of a bare io
 error. -->
 <!-- TODO(ruuda): The systemd status report after a deploy now includes the
 status command's stderr, so an unknown unit shows its reason ("Unit x could
 not be found") instead of a blank block. -->


 * List which specific systemd units failed to become active, if any.
 * Improve the error message for when symlink reconcile fails to remove an
   existing path.
 * Subdirectories in the `systemd` directory (used for systemd drop-ins) are
   now handled correctly, see the [directory layout
   chapter](directory_layout.md#systemd).
 * It is now possible to remove all apps from a host by leaving the host’s
   directory empty in the config tree. Previously this required creating a
   placeholder file in the directory as a workaround.
 * Deptool now reports IO errors that happen on the target host with more
   context, in particular errors always include the path involved in the failed
   operation.
 <!-- TODO(ruuda): With --limit, the up-to-date message from deploy and diff
 reads "All selected hosts" instead of claiming the whole cluster. -->
 <!-- TODO(ruuda): Same for sync --changed: with --limit, the no-changes
 message reads "No selected hosts" instead of claiming the whole cluster. -->


Bugfixes:

 <!-- TODO(ruuda): An empty --limit (an empty value or only commas) is now
 rejected, instead of silently widening the command to every host. -->
 <!-- TODO(ruuda): A symlink or special file in the config tree is now
 rejected with its path, instead of panicking (inside an app) or being
 silently dropped (at the host or app level, where it then planned as a
 removal). -->
 <!-- TODO(ruuda): Corrected misleading messages: reapply on a never-deployed
 host now suggests 'deptool deploy' (not only 'deptool sync', a dead end); and
 re-running 'deptool init' on an existing store reports "Using existing"
 instead of "Initialized". -->
 * Fix a bug where an inactive unit could be masked by an active one when
   multiple systemd units were affected in the same deployment.
 * Files in the config tree that are outside app directories (e.g. a readme per
   host) are no longer committed to the store, and no longer cause empty plans
   for a host when only such files changed.
 * Managed systemd units that are enabled are now re-enabled on every deploy.
   This fixes a bug where units could fail to start after a reboot. This happens
   because `systemctl enable` resolves symlinks, in particular it resolves
   `/var/lib/deptool/apps/<app>/current` to a fixed version. This means the
   symlink that systemd creates in `.wants` becomes stale after deploying a new
   version, and may even point to a version that got garbage-collected after
   a later deploy. Re-enabling after every change ensures that the `.wants`
   symlinks point to the correct versions.
 * In addition in order to ensure that enabled units start after a reboot, make
   symlink reconciliation less aggressive. Deptool no longer removes symlinks in
   `.wants` directories created by systemd. It now only removes symlinks that
   point through `current`, as those are the only symlinks Deptool creates.
 * Trigger `systemctl daemon-reload` after an app with systemd unit changes.
   Previously it was triggered only when units were enabled or disabled.
 * Fix a bug where an existing symlink that points to the correct target
   directory, caused deployment to fail with a vague _Is a directory_ error.
   This could happen in particular when a rollback-unsafe deployment created the
   symlink. A retry of that deployment would try to test if the contents of the
   target path already matched the contents of the source path, but this failed
   because the target path is a directory rather than a file. Now Deptool skips
   symlinks that already point at the correct source, and it reports a clear
   error when the target path is a directory.

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
