# Initial setup and metadeployment

Deptool is _agentless_, in the sense that it does not require any daemon to be
running on the target host. (Like Ansible, unlike Salt.) Furthermore, Deptool
works without requiring any specific setup step on the target host. For its
interactive deployment protocol it does require a `deptool` binary to be present
on the target host, but Deptool itself takes care of deploying it to the target
if it's not yet available, and updating it if it's not the correct version.

## Filesystem layout

On the target host, Deptool creates `/var/lib/deptool`, and it assumes exclusive
ownership over it; nothing should make changes to this directory except Deptool
itself. Inside there we have a few directories:

    /var/lib/deptool/store      The bare Git repository with a copy of the store
    /var/lib/deptool/bin        Deptool binaries
    /var/lib/deptool/apps       Config for apps deployed through Deptool

Inside the `bin` directory we store `deptool` binaries with a version suffix,
plus an optional suffix to disambiguate development versions, e.g.
`deptool-0.1.0-6116850ed6`. Upon installing a new binary, Deptool symlinks
`/usr/bin/deptool` to the binary in `/var/lib/deptool/bin`.

The optional suffix is included during development to be able to test without
having to bump the version all the time. It may be based on the short commit
hash of the commit that `deptool` was built from, if available, or we could set
it to a prefix of the the sha265sum of the binary itself.

The suffix ensures that both sides use the same binary, which ensures that they
are compatible. The downside of using the sha256sum of the binary is that we
can't do cross-architecture deploys (e.g. the operator is on OpenBSD, but the
target host is Linux), but this is not a use case we need to solve right now.

## Remote agent

When Deptool runs `deptool agent` on the remote host, it always starts
`deptool` by absolute path, so we know exactly which version we get, and it's
the same version that the operator is using. As an additional verification, the
agent announces its version as the hello message of the session protocol. The
advantage of this is that we know exactly what we run remotely. The disadvantage
is that if a newer version of Deptool deployed against the host and now we
deploy against it with an older version, we are not able to detect that. But
that's not a problem we have right now, so it's not something to worry about at
this stage.

## Installation

When the correct version of `deptool` does not exist on the target host, it
needs to be installed. We do this by starting an SSH session against the target
host which runs a shell command made up of these parts:

 - `sudo mkdir -p /var/lib/deptool/{bin,apps,store}`
 - `sudo dd of=<target-bin-path>`
 - `sudo chmod +x <target-bin-path>`
 - `sudo sha256sum <target-bin-path>`

Then we write the binary over stdin. This command is carefully chosen such that:

 - It makes minimal assumptions about the target host. We need sudo plus widely
   available coreutils, no binaries like `rsync` that may not be present.
 - Establishing an SSH connection is expensive, we pack everything in a single
   command to eliminate handshake latency.
 - The checksum at the end enables us to verify that the copy was successful,
   so we can retry if something went wrong.
 - It can run as a regular user who has `sudo` privileges, we don't need to SSH
   as root.
 - The commands are fairly simple, we avoid relying on complex and fragile logic
   in shell scripts, and we sidestep SSH shell escaping traps by having simple
   predictable file paths.
 - The command is idempotent, and it works regardless of whether Deptool is
   already installed or not, also if a different version is installed.

If this is the initial installation, then it's insufficient to put the binary in
place, we also need to `git init --bare` the store. `deptool agent` can
do this at startup if it detects that the store is not yet initialized.

## Version discovery

When Deptool connects to a target host, it always runs `deptool agent`.
If this command fails because the binary does not exist, we execute the
installation as described above, and then retry starting an agent session. This
means that for a new host we need three SSH sessions, but adding hosts and and
updating Deptool is rare compared to deployments itself, so the additional
latency is acceptable, and installation is necessary so we can't save much
latency either way. A user can configure SSH to use `ControlMaster` to avoid
session overhead if desired.
