# Directory layout

Deptool deploys _apps_: collections of related configuration files. Usually they
are related to a single application such as Postgres or Prometheus, but in
principle a Deptool app is just a collection of files deployed atomically. Apps
get deployed to _hosts_, and the collection of hosts is called a _cluster_.

Cluster configuration is defined by a _config tree_. A config tree is a two-tier
directory structure that defines the hosts, and apps per host. You materialize
this config tree on your filesystem, then commit it to the _store_ with `deptool
commit`, and finally `deptool deploy` applies it to the cluster.

The example config tree below defines two hosts, `dns01` and `web01`, and each
host contains one app — `nsd` and `nginx` respectively.

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

## Hosts

The name of a host in the config tree must be a hostname that is reachable
through `ssh <host>`. Either because it has a <abbr>DNS</abbr> name, or because
the host is defined in your `~/.ssh/config`. As an additional safeguard,
Deptool verifies that the contents of `/etc/hostname` on the target host match
the hostname used to initiate the connection.

## Apps

The full directory tree of an app gets deployed to
`/var/lib/deptool/apps/<appname>/current` on the target host.
Within an app directory, the paths below have special meaning. Their special
meaning does not prevent them from being deployed to the target host.

### manifest.json

Besides putting files on hosts, Deptool can manage daemon and runtime state,
like enabling and starting systemd units. This is specified in an optional
_manifest_. See the [manifests reference](manifests.md) for the full format.

### systemd/

This directory contains _available_ systemd units. For every file in this
directory, Deptool will create a symlink in `/etc/systemd/system` that points to
it. The symlink points through the app’s `current` symlink. While placing units
in this directory makes them available to systemd, they need to be
activated/enabled separately, see
[`units_enabled`](manifests.md#systemdunits_enabled) in the manifest.

<!-- TODO(ruuda): Document sysusers/ directory, analogous to systemd/ but for /etc/sysusers.d/. -->
