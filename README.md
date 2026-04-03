# Deptool

> [!Warning]
> This application is developed with the help of AI.

Deptool is a simple deployment aimed at managing a handful of applications on a
handful of machines. Like Ansible or similar tools it manages configuration on
machines. Unlike Ansible and like NixOS, it is declarative and mostly immutable.

Deptool works well with Flatcar Linux: It requires no dependencies to be
available on the host.

Deptool manages _clusters_. A cluster is a set of _hosts_. Deptool manages
configuration in two layers:

 * An _app_ is a related collection of configuration files, usually for a
   single application, specialized for a single machine. Apps are versioned.
 * A _config_ specifies the entire cluster: the set of hosts, and per host
   the apps that are deployed on them, and the app versions. The config as a
   whole is versioned too.

Together, this is how you define the desired state of your entire cluster.
Changes to the desired cluster state are tracked atomically (backed by Git, more
on that below). Deployment is more fine-grained however. Per machine, Deptool
compares the set of apps currently deployed on that machine against the
desired set. If there is a difference, it aligns the apps with the desired
state. Changing (or enabling or disabling) a single app is atomic, but
updates to multiple apps on the same machine are not atomic. This strikes a
balance between immutability and efficiency. To update one application, we don't
need to replace the entire world with a modified copy, we only need to touch
that single app.

After Deptool enables or updates an app on a machine, it has the ability to
restart or reload an associated systemd service.

## Push-based

Deptool runs are initiated by the operator from their developer machine. Deptool
then establishes SSH connections with the machines that are part of the cluster,
collects their current states, and if the desired configuration is newer than
what is present on the machine, it applies updates.

The fact that it's push-based does not mean that no agent runs on the target
machines in the cluster. In fact, a statically linked agent binary is how
Deptool can be efficient: unlike Ansible, it does not have to copy over
lots of Python code on every connection. It can also avoid the fragility of Bash
over SSH. Deptool just starts the agent remotely over SSH, and then speaks its
own protocol against the agent, the same way that e.g. Git works.

## Configuration Store

The desired cluster state is tracked in a Git repository. This enables us to
leverage Git's diffing capabilities to see what will change when we apply a
configuation change. It also gives us a convenient way to efficiently transport
trees of files and to materialize them on disk.

When Deptool runs, it acts based on the configuration store on the operator's
developer machine, but of course it can be synced to remotes. This is how
multiple operators can collaborate on the same cluster: by sharing the same
store.

When Deptool runs and it visits a machine, it first queries the current state
version present on that machine. With Git's commit graph it is possible to
determine if that state is an ancestor of the desired state. If it is, we can
deploy the newer config. If it's not an ancestor, something is wrong (possibly
the operator is running from an outdated store), and deployment against this
machine should abort.

## Future work

 - Error handling still feels clunky and verbose in code, and the messages are
   very bare-bones. Can we make the code less polluted by error handling, and
   at the same time get prettier errors?
 - Tests are still very verbose with lots of setup/teardown, which detracts from
   what they do. E.g. in deploy.rs `lock_push_pack_and_apply_...`. Can we build
   some abstractions to make tests easier to express?
 - Parallel execution against multiple hosts.
 - GC `/usr/lib/deptool/bin` when we install a new version.

## License

Deptool is licensed under the [Apache 2.0][apache2] license.
Please do not open an issue if you disagree with the choice of license.

[apache2]: https://www.apache.org/licenses/LICENSE-2.0
