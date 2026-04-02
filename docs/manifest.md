# Manifests

Deptool deploys _apps_, a collection of related configuration files, usually
related to a single application such as Postgres or Prometheus, but in principle
a Deptool app is just a collection of files deployed atomically.

Putting files on target hosts is not sufficient though, Deptool also needs to
manage some daemon and runtime state, like enabling and starting systemd units.
This is specified through a _manifest_ that is tracked in the [store](store.md).

TODO: Currently we have `systemd.json` that lists `units_enabled`. Probably we
should change this to `manifest.json`, which then has

```json
{
  "systemd": {
    "units_enabled": ["a.service", "b.timer"]
  }
}
```

Then later we can add other features to this manifest. For example, we could
have a section

```json
{
  "symlinks": {
    "/etc/frobnicator.conf": "frobnicator.conf"
  }
}
```

where the keys are paths on the target host's root filesystem, and the values
are relative paths inside the app's checkout. That's for a future version
though, right now we do not need this.
