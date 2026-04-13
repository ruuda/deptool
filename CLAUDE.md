## Interaction with me

 - Be brief.
 - Don't throw walls of text at me. Break things down in steps and ask me one thing at a time.

## Agent notes

 - Use quiet modes to avoid polluting your context window with irrelevant tool output. E.g. use `--quiet` on `cargo {test,check,fmt}`.
 - By your nature you are overconfident in your knowledge. Don't trust, verify. Read man pages, check tool behavior where possible.
 - However, tool calls are not a substitute for thinking. Form a hypothesis before verifying.
 - You are running inside a VM which is different from the production environment.
 - Avoid cryptic Bash tool calls, they are hard for me to review for permission checks.
 - Always use `rcl` for json processing, it's better suited for this than `jq`.

## Project details

 - This tool is for expert users (me) who can debug and understand the source. I will be watching the tool while it runs.
 - That means that a crash is _relatively_ not as bad as in a long-running daemon.
 - Therefore, optimize for code simplicity and readability over fancy tool output.
 - It needs to work for me on my laptop and cluster. This is not a generic tool that needs to work for every possible user on the planet.
 - We build incrementally with a short feedback loop. Make it work first, we'll make it fancy later. Do not prematurely complicate or generalize, make it easy to change later.

## Working through a task

 - Keep testability in mind from the start. Functional approaches (pure core, IO at the edge) are often more feasible to test than imperative code.
 - Do not pull in external dependencies without permission. Permission will only be granted if there is a good justification.
 - The docs are not law. If we discover design flaws while implementing, we can stop and change the design.
 - For large tasks, run `git diff` at the end and review your own work. It is very unlikely that you got a perfect version on your very first iteration, usually there are substantial things to change.
 - I will review your changes afterwards, so optimize for small reviewable diffs. Do not change comments or code without a good reason.
 - If it gets complex, typecheck at intermediate points with `cargo check --quiet`.
 - If the changes touch a test or code covered by tests, confirm with `cargo test --quiet`.
 - If the changes are not covered by a test, ask yourself, should they be? Not everything makes sense to test.
 - Run `cargo fmt --quiet` at the end on Rust code, `black --quiet` on Python code.
 - After a task is complete (I will tell you when it is, after you address my comments), reflect on the conversation. Are there any generic learnings? Update CLAUDE.md or your memory to prevent future instances of you from making the same mistakes.

## Reviewing your own work

Review at multiple levels, from high to low:

 1. Does the new behavior actually solve the problem we set out to solve?
 2. Does the diff implement the proposed solution? Are there logic bugs?
 3. Do the structs, methods, functions make sense? Could the call graph be simpler? Is there duplication?
 4. Local code quality: complex chains that could be a match? Comments stating the obvious? Missing justifications?

At every level, ask: is this complexity inherent, or an artifact of how I implemented it?

Specific checks:
 - Is it correct?
 - Can it be simpler or more elegant?
 - Code is a liability, can we achieve the same with less code? A "simplification" that adds lines probably isn't one.
 - Does new code duplicate something that already exists in the codebase?
 - Is it obvious to a reader with little context? Can it be made more obvious?
 - Did I preserve all why-comments from the original code?

## Working with Git

 - Git is available.
 - Do not commit. I will do that at logical points on our behalf.
 - Before embarking on a large task, record the current Git head, so you can later review what you did against that commit.
 - For large tasks, check the intermediate status with `git diff --shortstat` or `git diff --numstat`.
 - Negative diffstats are good. Codebase growth needs to be justified. Ask yourself whether the lines spent are well-spent.
 - Don't game the line stats. Redability is more important than line count.

## Code style

 - Optimize for readability.
 - Aim for self-documenting code, use comments when the purpose or workings of a piece of code is not obvious.
 - Simpler is more readable than complex.
 - Linear is more readable than branchy.
 - Name things after what they are and do, not after their purpose.
 - A reader who does not know the function args by heart can't tell what a call site like `frobnicate(true, None, 32)` does. Extract arguments into named variables when needed, prefer enums with descriptive names if possible.
 - At the call site, `frobnicate(true)` is meaningless but `frobnicate(FrobMode::IncludeWidgets)` is self-documenting.
 - Order function arguments from least-varying to most-varying. Configuration and context arguments (like a directory path) go before data arguments (like the specific changes to apply).
 - Prefer plain `match` over fancy method chains.
 - Prefer `match f() { Ok(v) => ..., Err(e) => ... }` over `if let Err(e) = f()` when the ok-path is the important one -- `if let Err` buries the function call after the error handling keyword.
 - Prefer making invalid states unrepresentable in the type system over excessive reliance on tests.
 - Prefer linear data ownership over shared mutable state. If you're reaching for a Mutex, first ask whether restructuring ownership would eliminate the sharing.
 - Measure before optimizing. Build the simplest correct version, benchmark it, and only add complexity if the measurements show a real problem.
 - Property-based tests are better than mere examples.
 - Prefer global imports over excessive qualification for types.
 - In assertions and `.expect()`, the message is the thing you expect to be true.
 - Doc comments should have a 1-line summary that fits in 80-ish columns, and then optionally a body separated from the summary by a blank line.
 - Use `.expect()` for logically impossible states and programming errors. Reserve `Result`/`Error` for expected runtime failures.
 - Comments start with a capital and use regular punctuation. Use *stars* for emphasis, not ALL CAPS. If you must use em-dashes at all, write them as -- in comments (but not in doc comments).
