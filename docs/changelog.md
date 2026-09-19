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

<!-- TODO(ruuda): Compatibility: the agent now records a store format version
(`deptool.storeVersion = 1`) in the host store when it takes the deploy lock,
and refuses to deploy to a host whose store records a version it does not
support ("store format version N is too new"). Groundwork for the per-host
history format in a later release. -->

<!-- TODO(ruuda): Bugfix: when the agent crashes in the middle of a session,
the operator now sees "agent exited unexpectedly" with the agent's stderr
(the panic message), instead of "protocol error: unexpected lock response:
None". -->

<!-- TODO(ruuda): Compatibility (1.3): per-host history, store version 2. The
agent converts a 1.2 store at lock, which costs one stale deploy. A rename no
longer needs the store deleted. `main` is no longer written. Commit messages
gain a `Deploy-Run` trailer. -->

### New features

 * `deptool diff` now accepts [`--word-diff`](cmd/deptool_diff.md#-word-diff)
   and [`--word-diff-regex`](cmd/deptool_diff.md#-word-diff-regex-regex) for
   more control over how the diff is rendered, in particular to inspect the diff
   for changes in files with long lines.

## 1.1.0

Released 2026-09-02. This release incorporates improvements and bugfixes from
four months of production usage.

### Compatibility

 * Symlinks into `/etc/systemd/network` and `/etc/containers/systemd` are no
   longer allowed directly. They have been superseded by the dedicated
   [`networkd`](directory_layout.md#networkd) and
   [`quadlets`](directory_layout.md#quadlets) directories, which trigger a
   configuration reload in addition to linking the files in place. Deploying
   with the new version handles this update transparently.
 * The remote installation command that `deptool` runs on the target system has
   changed: it removes shell brace expansion, to improve compatibility with
   shells that do not support it. This can affect users who pin the exact
   command used by Deptool in the `sudoers` file.

### New features

 * Add support for [systemd-networkd config](directory_layout.md#networkd).
   Deptool executes `networkctl reload` after a managed networkd file
   changes.
 * Add support for [Podman quadlets](directory_layout.md#quadlets).
 * Add [`deptool reapply`](cmd/deptool_reapply.md), a command that re-applies
   the currently deployed revision on every host in the cluster. This command
   should not be needed normally, but it helps to recover from manually created
   drift and bugs.
 * Add support for building on OpenBSD amd64. Thanks to Mikołaj Kucharski for
   contributing this feature.

### Improvements

 * The handling of systemd restarts has been improved with more reliable failure
   detection. Deptool now also restarts units simultaneously, and it reports
   which units it is waiting for during a deploy. See the new [_Check systemd
   unit status_](deployment_phases.md#check-systemd-unit-status) section for the
   details of the new behavior.
 * Deptool now performs additional safety checks that detect when the hostname
   of a managed host has been changed. This prevents misinterpreting the
   configuration deployed on the host, which will still hold the old name.
   See the new [_Renaming a host_ chapter](renaming_hosts.md) for how to handle
   renames.
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
 * The `sync` and `ping` commands now exit with a non-zero exit code if there
   is an error on any host, just like `deploy` already did.
 * An empty `--limit` is now an error, instead of silently behaving as if no
   limit was passed and applying against the entire cluster.

### Bugfixes

 * Fix two bugs that each prevented systemd units enabled by Deptool from
   starting after a reboot. See the [_Update systemd
   units_](deployment_phases.md#update-systemd-units) section for details about
   the new behavior.
 * Files in the config tree that are outside app directories (e.g. a readme per
   host) are no longer committed to the store, and no longer cause empty plans
   for a host when only such files changed. For hosts that had such files, the
   first deploy with the new Deptool version can still show an empty plan.
 * Trigger `systemctl daemon-reload` after an app with a systemd unit changes.
   Previously it was triggered only when units were enabled or disabled.
 * Avoid a scenario where an existing symlink that points to a directory could
   prevent Deptool from reconciling symlinks with an “Is a directory” error.
 * A symlink or special file in the config tree is now rejected with a clear
   error, rather than crashing Deptool during the planning phase.
 * Running Deptool with a config tree directory that does not exist, now fails
   with a clear error, and no longer saves the bad path as the default cluster
   for subsequent runs.
 * Validate the contents of the `BUILD_COMMIT` environment variable at build
   time to avoid a runtime crash in `deptool --version`.
 * Correct output messages that could be misleading or not technically correct
   in certain situations.

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
