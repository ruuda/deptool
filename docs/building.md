# Building

Deptool is written in Rust and builds with Cargo. It specifies a compatible
toolchain in `rust-toolchain.toml`, though other versions may work. If you
manage Rust with [Rustup][rustup], it will automatically download the right
toolchain.

Start by cloning the repository from one of the two mirrors:

```
$ git clone https://codeberg.org/ruuda/deptool
$ git clone https://github.com/ruuda/deptool
$ cd deptool
```

For local development, `cargo check` and `cargo test` work fine. For production
use though, because Deptool copies itself to the target host to run in agent
mode there, we need to build a static binary. There are two ways to do this.
With Cargo:

```
$ make release
$ ldd target/x86_64-unknown-linux-musl/release/deptool
```

Or with [Nix][nix]:

```
$ nix build
$ ldd result/bin/deptool
```

[rustup]: https://rust-lang.org/tools/install/
[nix]:    https://nixos.org/

