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

 * Add support for [Podman quadlets](directory_layout.md#quadlets).
 * Print stderr when agent installation fails.
 * Do not depend on shell brace expansion when installing the agent,
   for wider compatibility.
 * List which specific systemd units failed to become active, if any.
 * Fix a bug where an inactive unit could be masked by an active one when
   multiple systemd units were affected in the same deployment.
<!-- TODO(ruuda): Symlink reconcile failures (units, quadlets, sysusers) now
     name the link path in the error instead of printing a bare OS message
     like "Is a directory". -->

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
