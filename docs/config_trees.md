# Config tree

Deptool deploys _apps_: collections of related configuration files, usually
related to a single application such as Postgres or Prometheus, but in principle
a Deptool app is just a collection of files deployed atomically. Apps get
deployed to _hosts_, and the collection of hosts is called a _cluster_.

Cluster configuration is defined by a _config tree_. A config tree is a two-tier
directory structure that defines the hosts and apps per host. You materialize
this config tree on your filesystem, then commit it to the _store_ with `deptool
commit`, and finally `deptool deploy` deploys it to the cluster.

## Hosts

The name of a host in the config tree must be a hostname that is reachable
through `ssh <host>`. Either because it has a <abbr>DNS</abbr> name, or because
the host is defined in your `~/.ssh/config`. As an additional safeguard,
Deptool verifies that the contents of `/etc/hostname` on the target host match
the hostname used to initiate the connection.

## Apps

In the filesystem tree of an app, [some paths have special
meaning](directory_layout.md#special-paths). For things that cannot be
configured using a filesystem tree (symlinks to create, systemd units to
activate), an app can optionally contain a [manifest](manifests.md).
