# Deptool

Deptool is a declarative configuration deployment tool. It manages configuration
files on a cluster of unix hosts reachable over SSH. Deptool is designed for
smallish clusters (1–50) hosts managed by a small group of operators
(1–5 people), and it is extremely fast for this use case.

Check out [the manual](https://docs.ruuda.nl/deptool/) for more information.
<!-- The link above is the rendered version of the docs directory in this
repository. -->

## Hacking

Deptool is written in Rust and builds with Cargo. To typecheck and run the tests:

```console
$ cargo check
$ cargo test
```

The tests are safe to run locally. They only operate on temp directories, they
don’t invoke `systemd` or `ssh`.

For a production build you need a static binary,
see [docs/building.md](https://docs.ruuda.nl/deptool/building/).

## AI usage disclosure

Deptool was built with the help of AI. The code is primarily written by an LLM,
but on every iteration, the full diff is thoroughly reviewed by me before
committing to the repository. The intent is to keep this a high quality codebase,
not vibecoded AI slop. The user-facing documentation is entirely written by hand,
because I wouldn’t want to force people to read AI slop in order to use a tool
intended for humans.

## License

Deptool is licensed under the [Apache 2.0][apache2] license.
Please do not open an issue if you disagree with the choice of license.

[apache2]: https://www.apache.org/licenses/LICENSE-2.0
