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
<!-- TODO(ruuda): Support systemd drop-in directories: files inside a
     `<unit>.service.d/` directory under `systemd/` are symlinked individually,
     so unmanaged drop-ins in the same directory are left untouched, and an
     emptied `.d` directory is pruned. -->

Improvements:

 * Do not depend on shell brace expansion when installing the agent,
   for wider compatibility.
 * List which specific systemd units failed to become active, if any.
 * Improve the error message for when symlink reconcile fails to remove an
   existing path.
 * Print stderr when agent installation fails.

Bugfixes:

 * Fix a bug where an inactive unit could be masked by an active one when
   multiple systemd units were affected in the same deployment.
<!-- TODO(ruuda): Fix a bug where a content-only change to a unit file or
     drop-in that is not enabled did not trigger a daemon-reload, so systemd
     did not pick up the edit. -->
<!-- TODO(ruuda): Restarting an app now re-enables any of its units that lost
     their enablement (e.g. the `multi-user.target.wants/` symlink went missing
     after an interrupted deploy), repairing the drift instead of leaving the
     unit disabled until the next reboot fails to start it. -->

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
