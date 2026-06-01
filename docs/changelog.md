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
<!-- TODO(ruuda): Name the specific units in the activation-failure error,
     instead of "one or more units failed to become active". Also fixes a
     bug where a partial failure (some units up, one down) was reported as
     success, because `systemctl is-active` exits 0 if any unit is active. -->

 * Do not depend on shell brace expansion when installing the agent,
   for wider compatibility.

## 1.0.0

Released 2026-05-06.

This is the initial public release. It’s accompanied by [an announcement post][init-post].
The 1.0 version number represents the fact that the author uses this version
successfully for personal infra. It’s not a stability commitment, though if
future changes have compatibility impact, they will be clearly marked as such
in this changelog.

[init-post]: https://ruuda.nl/2026/deptool
