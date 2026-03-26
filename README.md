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

 * A _profile_ is a related collection of configuration files (usually for a
   single application), specialized for a single machine. Profiles are
   versioned.
 * A _cluster_ is a set of machines, and per machine the profiles that are
   enabled on them, and the profile versions.

Therefore, with Deptool you define the desired state of your entire cluster.
Changes to the desired state are tracked atomically (backed by Git). Deployment
is more fine-grained however. Per machine, Deptool compares the set of profiles
currently enabled on that machine against the desired set. If there is a
difference, it aligns the profiles with the desired state. Changing (or enabling
or disabling) a single profile is atomic, but updates to multiple profiles on
the same machine are not atomic. This strikes a balance between immutability and
efficiency. To update one application, we don't need to replace the entire world
with a modified copy, we only need to touch that single profile.

## Implementation

The desired cluster state is tracked in a Git repository. This enables us to
leverage Git's diffing capabilities to see what will change when we apply a
configuation change. It also gives us a convenient way to efficiently transport
trees of files and to materialize them on disk.

## License

Deptool is licensed under the [Apache 2.0][apache2] license.
Please do not open an issue if you disagree with the choice of license.

[apache2]: https://www.apache.org/licenses/LICENSE-2.0
