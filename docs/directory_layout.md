# Directory layout

Deptool expects cluster configuration in a two-tier directory structure.

 * Top-level directories are hosts.
 * Second-level directories are apps.

For example, the configuration below defines two _hosts_, `dns01` and `web01`, 
and each host contains one _app_ — `nsd` and `nginx` respectively.

    dns01
    └── nsd
        ├── example.com.zone
        ├── manifest.json
        ├── nsd.conf
        └── systemd
            └── nsd.service
    web01
    └── nginx
        ├── manifest.json
        ├── sites-enabled
        │   └── example.com.conf
        └── systemd
            └── nginx.service

## Deployment

For a given app, the full directory tree below it gets deployed to
`/var/lib/deptool/apps/<appname>/current` on the target host.

## Special paths

Within an app directory, the following paths have special meaning. Their special
meaning does not prevent them from being deployed to the target host into the
app directory.

### manifest.json

The app’s _manifest_ specifies additional properties beyond the files to deploy.
See the [manifests chapter](manifests.md) for the full format.

### systemd/

This directory contains _available_ systemd units. For every file in this
directory, Deptool will create a symlink in `/etc/systemd/system`. Units need
to be enabled separately, see [`units_enabled`](manifests.md#systemdunits_enabled).
