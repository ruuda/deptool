# Directory layout

Deptool deploys _apps_: collections of related configuration files. Usually they
are related to a single application such as Postgres or Prometheus, but in
principle a Deptool app is just a collection of files deployed atomically. Apps
get deployed to _hosts_, and the collection of hosts is called a _cluster_.

Cluster configuration is defined by a _config tree_. A config tree is a two-tier
directory structure that defines the hosts, and apps per host. You materialize
this config tree on your filesystem, and `deptool deploy` commits it to its
store and applies it to the cluster.

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
        ├── systemd
        │   └── nginx.service
        └── sysusers
            └── nginx.conf

## Hosts

The name of a host in the config tree must be a hostname that is reachable
through `ssh <host>`. Either because it has a <abbr>DNS</abbr> name, or because
the host is defined in your `~/.ssh/config`. As an additional safeguard,
Deptool verifies that the contents of `/etc/hostname` on the target host match
the hostname used to initiate the connection.

## Apps

The full directory tree of an app gets deployed to
`/var/lib/deptool/apps/<app>/current` on the target host.
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

### sysusers/

This directory contains [sysuser configuration files][sysusers.d], which enable
semi-declarative user and group management. For every file in this directory,
Deptool will create a symlink in `/etc/sysusers.d` that points to it. The
symlink points through the app’s `current` symlink. When a deploy changes the
contents of an app’s `sysusers` directory, Deptool executes
[systemd-sysusers][sd-sysusers].

> **Note**<br>
> Beware of systemd-sysusers’ behavior! In particular, it does nothing when a
> user or group already exists, so changing a sysuser file after its initial
> deployment is ineffective and can even cause confusion, as the file on disk
> will not match reality. Moreover, removing a sysuser file does not remove the
> users and groups it defines.

Sysusers are created before restarting systemd units, so that units can
reference users defined by sysusers included in the app, see also
[deployment phases](deployment_phases.md#update-sysusers). See the [user
management guide](user_management.md) for alternative ways to manage users.

[sysusers.d]:  https://www.freedesktop.org/software/systemd/man/latest/sysusers.d.html
[sd-sysusers]: https://www.freedesktop.org/software/systemd/man/latest/systemd-sysusers.html#
