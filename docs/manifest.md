# Manifests

Deptool deploys _apps_, a collection of related configuration files, usually
related to a single application such as Postgres or Prometheus, but in principle
a Deptool app is just a collection of files deployed atomically.

Putting files on target hosts is not sufficient though, Deptool also needs to
manage some daemon and runtime state, like enabling and starting systemd units.
This is specified through a _manifest_ (`manifest.json`) that is tracked in the
[store](store.md).

## Format

```json
{
  "systemd": {
    "units_enabled": ["a.service", "b.timer"]
  },
  "symlinks": {
    "/etc/frobnicator.conf": "frobnicator.conf"
  }
}
```

All sections are optional and default to empty.

### systemd

Lists which units from the app's `systemd/` directory should be enabled. All
units in `systemd/` are symlinked into the unit directory, but only those listed
here receive `systemctl enable --now`.

### symlinks

Maps absolute paths on the target host to relative paths inside the app's
checkout. Deptool creates symlinks at the specified paths, pointing through
`<app>/current/` so they survive reboots and always resolve to the active
version.

## Incremental adoption

Deptool is designed for incremental adoption. When a symlink target already
exists as a regular file on the host, Deptool compares its contents against
the source file. If they are identical, the file is replaced with a symlink.
If they differ, the deploy fails with an actionable error message, so the
operator can resolve the conflict manually.
