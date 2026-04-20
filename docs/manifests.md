# Manifests

Besides putting files on target hosts, Deptool can manage some daemon and
runtime state, like enabling and starting systemd units. This is specified
through a _manifest_, stored in [`manifest.json`](directory_layout.md#manifestjson).

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

### systemd.units_enabled

Lists which units from the app's `systemd/` directory should be enabled. All
units in `systemd/` are symlinked into the unit directory, but only those listed
here receive `systemctl enable --now`.

### symlinks

Maps absolute paths on the target host to relative paths inside the app's
checkout. Deptool creates symlinks at the specified paths, pointing through
`<app>/current/` so they always resolve to the active version.

## Incremental adoption

Deptool is designed for incremental adoption. When a symlink target already
exists as a regular file on the host, Deptool compares its contents against
the source file. If they are identical, the file is replaced with a symlink.
If they differ, the deploy fails with an actionable error message, so the
operator can resolve the conflict manually.
