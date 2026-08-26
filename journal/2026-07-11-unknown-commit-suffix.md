# Fix `deptool --version` panic on short/unknown `BUILD_COMMIT`

## Topic

`deptool --version` crashed with `end byte index 10 is out of bounds for
string of length 4` when the binary was built with `BUILD_COMMIT=unknown`
(dirty tree). The user wants short, empty, or arbitrary commit values
supported, not rejected.

## Summary

- **Root cause**: four leaf sites sliced `&BUILD_COMMIT[..10]`
  (`src/main.rs` x2, `src/error.rs`, and the `--version` print). The slice
  assumes a full 40-char hash; `unknown` is 7 chars.
- **Fix**: `setup::build_commit_suffix(commit: &str)` caps at
  `commit.len().min(10)` instead of slicing at 10. All four call sites use
  it. `build.rs` and `flake.nix` are unchanged — any caller value is now
  legal; the only build.rs change is a comment documenting that.
- **Tests**: `suffix_caps_at_ten` and `suffix_of_short_commit_is_whole_commit`
  in `src/setup.rs`. The second one is the regression test for this bug
  (`"unknown"` input). Both test the pure fn, not the constant, since the
  constant's value is fixed at compile time.
- Changelog TODO marker added; prose left for Ruud.

## Decisions and rejected paths

- Rejected: assert `commit.len() >= 10` in build.rs. Fails the user's
  requirement (unknown/empty must work).
- Rejected: a `&'static str` wrapper around the constant for call sites.
  A test that only asserts on the wrapper can never fail; the pure
  `build_commit_suffix(commit)` is the testable core, and the call sites
  pass the constant explicitly.
- Rejected: make build.rs itself emit `unknown` for dirty debug trees.
  That was the original instinct, but it changes identity semantics for
  existing debug builds; the user's ask was only that consumers tolerate
  any value.
- Rejected: a `&'static str` wrapper fn. The constant can't express the
  short-commit case in tests, so the pure fn is the testable core.

## Caveats

- No Rust toolchain in this VM; `cargo test` and `cargo fmt` not run.
- `flake.nix` still throws "must be built from a clean tree" when `rev` is
  absent. Left as-is: it's a deliberate guard, and `unknown` is now safe
  if it ever changes.
- `build/<platform>.sh` does `git rev-parse HEAD | cut -c-10` — a dirty
  tree still yields a real short hash there, not `unknown`.
