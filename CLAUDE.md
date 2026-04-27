## Interaction

 - Be brief. Don't throw walls of text. Break things down in steps.
 - Ask one question at a time. Don't bundle multiple questions.
 - Don't acknowledge corrections ("You're right", "Good point"). Just act on them.
 - Use Blade Runner names in examples, not the user's real name.

## Agent notes

 - Use quiet modes to avoid polluting your context window. E.g. `--quiet` on build/test commands.
 - By your nature you are overconfident in your knowledge. Don't trust, verify. Read man pages, check tool behavior where possible. You have a pattern of acknowledging rules in reflection but not following them during work -- knowing a rule exists is not the same as applying it. Slow down at decision points.
 - However, tool calls are not a substitute for thinking. Form a hypothesis before verifying.
 - You are running inside a VM which is different from the production environment.
 - Avoid cryptic Bash tool calls, they are hard to review for permission checks.
 - Always use `rcl` for json processing, it's better suited for this than `jq`.
 - Don't fabricate capabilities. Verify tool availability before claiming you can use something.
 - When the compiler warns, it's right. Investigate instead of suppressing.
 - Verify subagent work: check that agents committed on the right branch with actual changes.
 - Never use `find` or `grep` to navigate the repo, they waste context on untracked files. Use `git grep` and `git ls-files` instead.

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
 - Before presenting code to the user, run the post-generation checklist below and fix what you find. Do not skip this step. Do not say "the code looks good" without having run `git diff` and walked every checklist item. Walking the checklist means articulating *why* each item passes for this change. If your check is "looks fine", that's rubber-stamping; you haven't actually checked. The checklist exists because these items are easy to miss when reviewing your own work — running it as ritual defeats the purpose.
 - I will review your changes afterwards, so optimize for small reviewable diffs. Do not change comments or code without a good reason.
 - If it gets complex, typecheck at intermediate points with `cargo check --quiet`.
 - If the changes touch a test or code covered by tests, confirm with `cargo test --quiet`.
 - If the changes are not covered by a test, ask yourself, should they be? Not everything makes sense to test.
 - Run `cargo fmt --quiet` at the end on Rust code, `black --quiet` on Python code.
 - Write failing tests first, then implement (red-green).
 - New code paths need new tests before claiming done.
 - After a task is complete (I will tell you when it is, after you address my comments), reflect on the conversation. Are there any generic learnings? Update CLAUDE.md or your memory.

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
 - Does new code duplicate something that already exists in the codebase? Look for existing functions before creating new ones.
 - Is it obvious to a reader with little context? Can it be made more obvious?
 - Did I preserve all why-comments from the original code? Grep for them before and after rewrites.

Post-generation checklist (run after writing code, before presenting):
 - For each doc comment: does it explain *why* this exists, not just *what* it does? Would a reader learn something they can't see from the signature?
 - For each inline comment: does the next line of code already say the same thing? If so, delete the comment.
 - For each error path: what does the operator/user actually see? Trace it to the display.
 - For each new function: are the arguments in the right order (stable before varying, old before new)?
 - For each `replace_all` edit: will it match inside longer lines at different indentation levels? When in doubt, edit distinct groups separately.
 - For each new test: what specific bug would it catch? If breaking the test would require an implementation change you'd obviously notice without the test, drop it or rework it to test a real property.
 - For each deleted test: where is that behavior tested now?
 - For each deleted or moved piece of code: what invariant or purpose did it serve at its original location? Is that still enforced?
 - For each bug fix: does the same bug exist in a shared function that this code calls? Fix it at the root, not the leaf.
 - For each match on a mode/flag: does the test verify *both* branches produce distinct behavior?
 - For each new parameter: is this data already available through another parameter (e.g. a field of a struct already being passed)?
 - For each new ref/file/IO operation: is there an existing function or pattern in the codebase that already does this? Use it.
 - For each `true`/`false`/numeric literal at a call site: would a reader know what it means? Extract to a named variable.
 - For each new `bool` parameter: use an enum instead. `frobnicate(true)` is meaningless; `frobnicate(FrobMode::IncludeWidgets)` is self-documenting.
 - For each `replace_all` on a word: will it match inside a longer word (e.g. "started" inside "restarted")? Use more context or targeted edits.
 - For each doc comment on changed code: does it still match the current signature and behavior? Re-read it after every refactor.
 - For each change to user-visible output (plan display, logs, errors): `git grep` for fragments of the old output in `docs/` and tests. Update every match.

## Working with Git

 - Git is available.
 - Do not commit unless asked. I will do that at logical points on our behalf.
 - Before embarking on a new change, confirm the current branch is `master`, then create a new branch with `git checkout -b`. Creating branches is allowed; committing still requires explicit instruction. Diff against `master` to review the change. I will merge back into master when done.
 - For large tasks, check the intermediate status with `git diff --shortstat` or `git diff --numstat`.
 - Negative diffstats are good. Codebase growth needs to be justified. Ask yourself whether the lines spent are well-spent.
 - Don't game the line stats. Readability is more important than line count.

## Session journal

 - Each session has a journal entry at `journal/YYYY-MM-DD-topic.md`. It is the bridge between sessions: a future session reads the journal to pick up context. Treat it as a working document, not an after-the-fact summary.
 - Update the journal *as the session progresses*, not only at the end. After a meaningful step (a design pivot, a framing tool the user introduces, a substantive review pass), revisit the entry. The entry should be in a usable state at any moment in case the session is interrupted.
 - What to capture: the session's key topics, a running summary of what was done (decisions and rejected paths, not a play-by-play), reflections and learnings, and any framing tools (personas, mental models) the user established. Existing entries in `journal/` show the shape, including "What went well" / "What I should do differently" sections.
 - What to skip: per-edit blow-by-blows (`git log` covers that), restating things already in code or docs, filler.
 - If a learning is generic — applies beyond this session's topic — also update `CLAUDE.md` or write a memory, per the rule in the task section.

## Code style

 - Optimize for readability.
 - Aim for self-documenting code, use comments when the purpose or workings of a piece of code is not obvious.
 - Comments explain *why*, not *what*. Don't state the obvious.
 - Simpler is more readable than complex.
 - Linear is more readable than branchy.
 - Name things after what they are and do, not after their purpose. Names must be clear without consulting their definition.
 - A reader who does not know the function args by heart can't tell what a call site like `frobnicate(true, None, 32)` does. Extract arguments into named variables when needed, prefer enums with descriptive names if possible.
 - At the call site, `frobnicate(true)` is meaningless but `frobnicate(FrobMode::IncludeWidgets)` is self-documenting.
 - Order function arguments from least-varying to most-varying. Configuration and context arguments (like a directory path) go before data arguments (like the specific changes to apply).
 - Prefer plain `match` over fancy method chains or `let-else`.
 - Prefer `match f() { Ok(v) => ..., Err(e) => ... }` over `if let Err(e) = f()`. The thing being done should come first; result handling reads after, not in the binding position. `match` keeps the flow linear; `if let Err(...) =` buries the call. (`if let Some(...)` on `Option` is fine.)
 - Prefer making invalid states unrepresentable in the type system over excessive reliance on tests.
 - Prefer linear data ownership over shared mutable state. If you're reaching for a Mutex, first ask whether restructuring ownership would eliminate the sharing.
 - Prefer safe APIs over unsafe. Check `std` and `std::os::unix` before reaching for `libc`.
 - Measure before optimizing. Build the simplest correct version, benchmark it, and only add complexity if the measurements show a real problem. When an optimization forces complexity on callers (new type bounds, split borrows, wrapper functions), the cure is worse than the disease.
 - Don't overengineer. The right design emerges by subtraction. Justify with the current problem, not hypothetical future needs.
 - When analogous code already exists, mirror its structure. Symmetry signals good design.
 - Place code in the module that owns the concept, not wherever it's called from.
 - When changing one side of a boundary, minimize changes to the other side.
 - Property-based tests are better than mere examples.
 - Tests read like behavior descriptions. Name tests after the property they assert (Hspec style). Extract lookups into helpers to reduce noise.
 - Prefer global imports over excessive qualification for types.
 - In assertions and `.expect()`, the message is the thing you expect to be true.
 - Use `.expect()` for logically impossible states and programming errors. Reserve `Result`/`Error` for expected runtime failures.
 - Don't annotate closure types when the compiler can infer them.
 - Use `.to_str().ok_or()` not `.to_string_lossy()` when loss would break the program.
 - Newtypes go all the way down. If a callee unwraps a newtype, it should accept the newtype instead.
 - Use named struct fields, not tuples, when fields have the same type. Named fields prevent silent swaps.
 - Don't reuse error variants for unrelated failures -- the Display output lies. Read the doc comment before reusing a variant.
 - Error construction stays where the error is detected. Checks belong inside the function that has the state, not in the caller.
 - Doc comments should have a 1-line summary that fits in 80-ish columns, then optionally a body separated by a blank line.
 - Comments start with a capital and use regular punctuation. Use *stars* for emphasis, not ALL CAPS. Write em-dashes as -- in comments.

## User-facing output (logs, plan display, errors)

 - Write for someone who has never seen the source code and does not know the tool's internals.
 - Prefer words over symbols. Symbols can be ambiguous or invisible in certain fonts.
 - Log near the mutation, not near the planning. If the plan and the execution are separate phases, log during execution.
 - One event, one message. Don't have different strings for the log and the protocol for the same error.
 - Project naming conventions (e.g. `Deptool` for the tool, `deptool` for the binary) apply here too. User-facing prose is user-facing prose, whether it's in docs or in an error string.

## Writing documentation

 - Follow Google's technical writing advice: active voice, clear sentences, define terms before using them.
 - Don't be verbose. Docs go out of date, so avoid details that change often.
 - Documented behavior must be consistent with the implementation. Verify against the code.
