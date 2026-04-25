# Building

Deptool is written in Rust and builds with Cargo. It specifies a compatible
toolchain in `rust-toolchain.toml`, though other versions may work. If you
manage Rust with [Rustup][rustup], it will automatically download the right
toolchain. Deptool has few dependencies, so it’s quick to build from source.

Start by cloning the repository from one of the two mirrors:

```
$ git clone https://codeberg.org/ruuda/deptool
$ git clone https://github.com/ruuda/deptool
$ cd deptool
```

For local development, `cargo check` and `cargo test` work fine. For production
use though, because Deptool copies itself to the target host to run in agent
mode there, we need to build a static binary. There are two ways to do this.

<!-- TODO(ruuda): Update this section. The Makefile is gone; cross-builds for
all release targets now go through `./build/release.sh`, which requires
cargo-zigbuild and the rustup targets installed. The script lays out
binaries under `target/deptool-bin/<platform>/<bin-name>` for the runtime
to find when `DEPTOOL_BIN_DIR` is set. The Nix build path also needs
revisiting (build.rs now reads `.git/HEAD` and `git status`). -->

If you have Rustup and a C compiler installed, the Cargo-based build should work
out of the box. To ensure a fully static build we need to set a few environment
variables and select `musl` as the target. The Makefile takes care of this:

```
$ make release
$ cp target/x86_64-unknown-linux-musl/release/deptool ~/.local/bin
$ ldd $(which deptool)
statically linked
```

Alternatively, you can build with [Nix][nix], which provides a self-contained
and reproducible build environment:

```
$ nix build
$ cp result/bin/deptool ~/.local/bin
$ ldd $(which deptool)
statically linked
```

[rustup]: https://rust-lang.org/tools/install/
[nix]:    https://nixos.org/
