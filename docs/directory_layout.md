# Directory layout

Deptool expects cluster configuration in a two-tier directory structure.

 * **Top-level directories are hosts.**
   The name of the directory must correcpond to a hostname that is reachable
   through `ssh <host>`. (Either because it has a <abbr>DNS</abbr> name, or
   because the host is defined in your `~/.ssh/config`.) As an additional
   safeguard, Deptool verifies that the contents of `/etc/hostname` on the
   target host match the hostname used to initiate the connection.
 * **Second-level directories are apps.**
   An app is a collection of related files that are deployed together, e.g.
   configuration for a webserver.

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

## Special paths

Within an app directory, the following paths have special meaning. Their special
meaning does not prevent them from being deployed to the target host into the
app directory.

### `manifest.json`

The app’s _manifest_ specifies additional properties beyond the files to deploy.
See the [manifests chapter](manifests.md) for the full format.

### `systemd/`

This directory contains _available_ systemd units. For every file in this
directory, Deptool will create a symlink in `/etc/systemd/system`. Units need
to be enabled separately, see TODO(manifest).
