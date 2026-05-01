# Deptool

Deptool is a declarative configuration deployment tool. It manages configuration
files on a cluster of unix hosts reachable over SSH. Deptool is designed for
small clusters (1–50 hosts) managed by a small group of operators (1–5 people).
It is extremely fast for this use case: it can show a deployment plan in
milliseconds, and execute it sub-second.

To get started, these are the most useful chapters from the manual:

<!-- The links below are the rendered versions of what's in the docs directory
     in this repository. -->
 * [Overview](https://docs.ruuda.nl/deptool/)
 * [Tutorial](https://docs.ruuda.nl/deptool/tutorial/)
 * [Directory layout](https://docs.ruuda.nl/deptool/directory_layout/)
 * [Deployment phases](https://docs.ruuda.nl/deptool/deployment_phases/)
 * [CLI reference](https://docs.ruuda.nl/deptool/cmd/deptool/)

## Status

 * Deptool is a hobby project without stability promise. I use it to manay my
   own personal infra, and it works very well for this use case.
 * I’m open sourcing it in the hope that others find it useful too, bug reports
   are welcome.
 * If there is sufficient interest, I may look into setting up a proper release
   flow with prebuilt binaries and more care for compatibility between releases.
   [Drop me a message][contact] if Deptool is useful to you!

[contact]: https://ruudvanasseldonk.com/contact

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

## LLM usage disclosure

Deptool was built with the help of LLMs. The code is primarily written by LLMs,
but I carefully review the entire diff and iterate until I am happy with the code
before committing to the repository. I want this to be a high quality codebase,
not vibecoded AI slop. The user-facing documentation and this readme are written
by hand, because even though LLMs could get the content right, you can tell an
LLM was involved, and that has [negative][cantrill-fly] [consequences][ruuda-llm].
I don’t want to force humans to read LLM-generated to be able to use a tool
intended for humans.

[cantrill-fly]: https://bcantrill.dtrace.org/2025/12/05/your-intellectual-fly-is-open/
[ruuda-llm]:    https://ruudvanasseldonk.com/2025/llm-interactions

## License

Deptool is licensed under the [Apache 2.0][apache2] license.
Please do not open an issue if you disagree with the choice of license.

[apache2]: https://www.apache.org/licenses/LICENSE-2.0
