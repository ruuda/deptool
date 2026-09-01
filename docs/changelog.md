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

### New features

 * Add support for [Podman quadlets](directory_layout.md#quadlets).
 * Add support for [systemd-networkd config](directory_layout.md#networkd).
   Deptool executes `networkctl reload` after a managed networkd file
   changes.
 * Add [`deptool reapply`](cmd/deptool_reapply.md), a command that re-applies
   the currently deployed revision on every host in the cluster. This command
   should not be needed normally, but it helps to recover from manually created
   drift and bugs.
 * Add support for OpenBSD amd64. Thanks to Mikołaj Kucharski for contributing.

### Improvements

 * The handling of systemd restarts has been improved with more reliable failure
   detection and increased parallelism. Deptool now also reports which units it
   is waiting for during a deploy. See the new [_Check systemd unit
   status_](deployment_phases.md#check-systemd-unit-status) section for the
   details of the new behavior.
 * Deptool now has additional safety checks to avoid unintended behavior when
   the hostname of a managed host has been changed. See the new [_Renaming a
   host_ chapter](renaming_hosts.md) for more details.
 * Subdirectories in the `systemd` directory (used for systemd drop-ins) are
   now handled correctly, see the [directory layout
   chapter](directory_layout.md#systemd).
 * It is now possible to remove all apps from a host by leaving the host’s
   directory empty in the config tree. Previously this required creating a
   placeholder file in the directory as a workaround.
 * List which specific systemd units failed to become active, if any.
 * Deptool now reports errors that happen on the target host with clearer
   messages and more context. In particular, <abbr>IO</abbr> errors always
   include the path involved in the failed operation, and stderr of commands
   that failed on the target host is forwarded to the operator.
 * Do not depend on shell brace expansion when installing the agent to improve
   compatibility, and print stderr when agent installation fails to aid
   debugging.
 * The `deploy` and `reapply` commands now prompt again for confirmation on
   invalid input, rather than treating any invalid input as “no”.
 * The `sync` and `ping` subcommand now exit with a non-zero exit code if there
   is an error on any host, just like `deploy` already did.
 * An empty `--limit` is now an error, instead of silently behaving as if no
   limit was passed and applying against the entire cluster.

### Bugfixes

 * Fix a bug where an inactive systemd unit could be masked by an active one
   when multiple systemd units were affected in the same deployment.
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
 * Furthermore, in order to ensure that enabled units start after a reboot, make
   symlink reconciliation less aggressive. Deptool no longer removes symlinks in
   `.wants` directories created by systemd. It now only removes symlinks that
   point through `current`, as those are the only symlinks Deptool creates.
 * Trigger `systemctl daemon-reload` after an app with a systemd unit changes.
   Previously it was triggered only when units were enabled or disabled.
 * Avoid a scenario where an existing symlink that points to a directory could
   prevent Deptool from reconciling symlinks.
 * A symlink or special file in the config tree is now rejected with a clear
   error, rather than crashing Deptool during the planning phase.
 * Running Deptool with a config tree directory that does not exist, now fails
   with a clear error, and no longer saves the bad path as the default cluster
   for subsequent runs.
 * Ensure that output messages are technically correct even when `--limit` is
   used, and correct a few more messages that could be misleading in certain
   situations.
 * Validate the contents of the `BUILD_COMMIT` environment variable at build
   time to avoid a runtime crash in `deptool --version`.

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
