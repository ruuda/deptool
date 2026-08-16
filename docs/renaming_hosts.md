# Renaming a host

A deployed host stores the commit that is deployed on it, and that commit
identifies the host's config by hostname. After you change a host's
`/etc/hostname`, the deployed commit only has config for the old name, and
`deptool deploy` refuses the host:

    web2: failed: deployed commit has no config for this host

There is no in-place rename; treat the rename like a reprovision. Delete the
store on the host, then deploy:

    ssh web2 sudo rm -rf /var/lib/deptool/store
    deptool deploy

The deploy treats the host as a fresh install: it deploys every app and
restarts every enabled unit. Deleting the store discards the host's
deployment history (its refs and reflogs), like a reprovision would. The
files under `/var/lib/deptool/apps` and the systemd units stay in place
until the deploy replaces them.

Apps that only existed under the old name are not stopped or disabled by the
fresh deploy. To drop apps from the host, remove them from the config tree
and deploy *before* the rename, so Deptool stops and cleans them up.
