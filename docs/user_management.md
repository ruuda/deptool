# User management

For a host that is managed by a configuration management system, we often need
to manage unix users on that host, for example to give every daemon its own
system user. There are several ways to go about this with Deptool.

## Manage users externally

One approach is to not involve Deptool at all. For example, on image-based
systems that configure the machine at first boot, such as [Flatcar Linux][flatcar]
through [Ignition][ignition] and [Butane][butane], perhaps it suffices to
let Ignition create the users you need.

Or maybe your cluster is small and you don’t mind performing some steps manually
as part of an infrequent one-time setup process. Sometimes making everything
fully declarative is overkill, and `useradd` goes a long way.

[ignition]: https://coreos.github.io/ignition/
[butane]:   https://coreos.github.io/butane/
[flatcar]:  https://www.flatcar.org/

## Manage the full userdb

On the other extreme end of the spectrum, we can fully manage the user database
with Deptool by making `/etc/passwd`, `/etc/shadow`, etc. managed files. If you
know _exactly_ which users and uids you need, you can pregenerate these files
and deploy them with Deptool, for example through an app named `users` that
defines a [symlink](manifests.md#symlinks) for `/etc/passwd` and related files.
This approach is incompatible with systems that have imperative parts, such as
Apt packages that create users upon installation.

## Systemd dynamic users

If the users you need are only needed for running daemons, and they don’t need
to own any directories except in [a few standard locations][runtimedirs], then
[`DynamicUser=`][dynamicuser] might suffice. Perhaps you don’t need any user
management outside of systemd units!

[runtimedirs]: https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#RuntimeDirectory=
[dynamicuser]: https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#DynamicUser=

## Systemd sysusers

Finally, Deptool integrates with [systemd-sysusers][sd-sysusers] to manage users
semi-declaratively. When you place [sysuser files][sysusers.d] in an app’s
[sysusers directory](directory_layout.md#sysusers), Deptool creates symlinks to
them in `/etc/sysusers.d`. Then it runs `systemd-sysusers` to materialize the
users and groups defined in those files.

[sysusers.d]:  https://www.freedesktop.org/software/systemd/man/latest/sysusers.d.html
[sd-sysusers]: https://www.freedesktop.org/software/systemd/man/latest/systemd-sysusers.html#
