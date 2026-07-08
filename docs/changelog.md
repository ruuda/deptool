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

New features:

 * Add support for [Podman quadlets](directory_layout.md#quadlets).

Improvements:

 * Do not depend on shell brace expansion when installing the agent to improve
   compatibility, and print stderr when agent installation fails to aid
   debugging.
 * List which specific systemd units failed to become active, if any.
 * Improve the error message for when symlink reconcile fails to remove an
   existing path.
 * Subdirectories in the `systemd` directory (used for systemd drop-ins) are
   now handled correctly, see the [directory layout
   chapter](directory_layout.md#systemd).

Bugfixes:

 * Fix a bug where an inactive unit could be masked by an active one when
   multiple systemd units were affected in the same deployment.
 * Files in the config tree that are outside app directories (e.g. a readme per
   host) are no longer committed to the store, and no longer cause empty plans
   for a host when only such files changed.
 * Managed systemd units that are enabled are now re-enabled on every deploy.
   This fixes a bug where units could fail to start after a reboot. This happens
   because `systemctl enable` resolves symlinks, in particular it resolves
   `/var/lib/deptool/apps/<app>/current` to a fixed version. This means the
   symlink that systemd creates in `.wants` is stale, and may even point to a
   version that got garbage-collected after a later deploy. Re-enabling after
   every change ensures that the `.wants` symlinks point to the correct
   versions.
 * Trigger `systemctl daemon-reload` after an app with systemd unit changes.
   Previously it was triggered only when units were enabled or disabled.

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
