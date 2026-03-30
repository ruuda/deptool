## General instructions

 - Be brief.
 - Prefer more small steps over throwing walls of text at me.
 - Keep the diff small if possible to aid review.
 - Do not Git commit, I will do that once I'm satisfied with a change.
 - Always use `rcl` for json processing, it's better suiter for this than `jq`.
 - Run `cargo test --quiet` to avoid verbose output. Do not run `cargo fmt`.
 - Feel free to update this file when I give relevant instructions.

## Current project priorities

 - We are rapidly prototying a tool that needs to work only for me, in my very specific situation. This is not a generic tool that needs to handle every edge case of everybody on the internet.
 - I am the only user and I aim to understand the codebase, so optimize for simple code over extensive error reporting.
 - We build incrementally. Start with the minimum viable thing, we'll extend it later. Do not prematurely generalize or complicate, we can't predict what parts will need to be changed later, just optimize for making things easy to change.
 - The readme is not law. If we discover flaws in the design while implementing, we can change the design.
 - Do pull in external dependencies without permission, which will only be granted if there is a good justification.

## Coding standards

 - Rust for the main "production" code, Python for utilities and test drivers etc. if needed.
 - Ensure Rust code is formatted with `cargo format` and Python code with `black`.
 - Ensure Rust code typechecks with `cargo check`, and Python with `mypy --strict`.
 - Prefer making invalid states unrepresentable in the type system over excessive reliance on tests.
 - Property-based tests are better than mere examples.
 - Testing things with side effects is hard. Separating IO from pure parts usually makes things easier to test, and it makes the tests faster and mores table.
 - In assertions and `.expect()`, the message is the thing you expect to be true.
 - Avoid the boolean parameter trap. At the call site, `frobnicate(true)` is meaningless but `frobnicate(FrobMode::IncludeWidgets)` is self-documenting.
 - Rust doc comments should have a 1-line summary that fits in 80-ish columns, and then optionally a body separated from the summary by a blank line.
