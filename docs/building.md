# Building

Prebuilt binaries are not yet available, so to use Deptool, you have to build it
from source. Deptool copies itself to the target host to run in agent mode
there, so we need binaries for the target hosts. If your target hosts use the
same platform (operating system and <abbr>CPU</abbr> architecture) as the
machine where you deploy from — for example, an x64 Linux laptop deploying to
x64 Linux servers — you only need to build one binary. Follow the _operator
side_ section below. For cross-platform deployment, such as an <abbr>ARM</abbr>
Macbook deploying to x64 Linux, read the _host side_ section as well.

## Prerequisites

Deptool is written in Rust and builds with Cargo. It has few dependencies, so
it’s quick to build from source. The repository includes a `rust-toolchain.toml`
that specifies a compatible toolchain, though other versions may work. If you
manage Rust with [Rustup][rustup], it will automatically download the right
toolchain.

Start by cloning the repository from one of the two mirrors:

```
$ git clone https://codeberg.org/ruuda/deptool  # Most stable mirror
$ git clone https://github.com/ruuda/deptool    # Alternative mirror
$ cd deptool
```

For local development, `cargo check` and `cargo test` work fine. For a release
build, `cargo build --release` works for running locally, but the resulting
dynamically linked binary might be incompatible with target hosts. For maximal
portability, follow the steps below to build a static binary instead.

[rustup]: https://rust-lang.org/tools/install/

## Building the operator side

The operator side is the side where you run `deptool deploy`. To build a
static release binary for x64 Linux:

    $ cargo build --release --target x86_64-unknown-linux-musl
    $ file target/x86_64-unknown-linux-musl/release/deptool
    ELF 64-bit LSB pie executable, x86-64, …, static-pie linked, …, stripped

Copy it to a directory on your `PATH` to use it:

    $ cp target/x86_64-unknown-linux-musl/release/deptool ~/.local/bin
    $ deptool --version

If you are on a different platform, you can build a dynamically linked release build:

    $ cargo build --release
    $ cp target/release/deptool ~/.local/bin
    $ deptool --version

If you want to deploy to hosts that run a different operating system or
<abbr>CPU</abbr> architecture than where you deploy from, you also need to build
the platform-specific binaries, as described in the next section. If you deploy
only to the same platform, then Deptool can use the same binary on all sides, so
we are done here.

## Building the host side

When your target hosts are a different platform than the operator side, we need
a binary per platform in [`DEPTOOL_BIN_DIR`][bindir]. For example, if you are on
an <abbr>ARM</abbr> Macbook deploying to x64 Linux, we need to get that Linux
binary from somewhere. If you have access to a build machine with the right
specifications, you can build there, but we can also cross-compile. This is
fairly straightforward with [`cargo-zigbuild`][czb]. The binaries in
`DEPTOOL_BIN_DIR` also need to be named after the commit hash they were built
from, to ensure that both ends of the protocol are compatible. The build scripts
in the `build` directory automate this. For example:

    $ build/linux-aarch64.sh
    $ build/linux-x86_64.sh

    $ tree target/deptool-bin
    target/deptool-bin
    ├── linux-aarch64
    │   └── deptool-0.1.0-f55bedb949
    └── linux-x86_64
        └── deptool-0.1.0-f55bedb949

You can now point `DEPTOOL_BIN_DIR` at `target/deptool-bin`, or you can copy
the contents of `deptool-bin` into `~/.cache/deptool` (the default location)
to ensure that your operator-side `deptool` binary can discover the
platform-specific binaries.

[bindir]: cmd/deptool.md#deptool_bin_dir
[czb]:    https://github.com/rust-cross/cargo-zigbuild

## Building with Nix

Alternatively, you can build with [Nix][nix], which provides a self-contained
and reproducible build environment. This currently only works for x64 Linux.

```
$ nix build
$ cp result/bin/deptool ~/.local/bin
$ ldd $(which deptool)
statically linked
```

[nix]: https://nixos.org/
