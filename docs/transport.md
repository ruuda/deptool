# Transport

Deptool transfers Git objects between the operator machine and target hosts
without requiring Git to be installed on the target. All data flows over the
same SSH session used for the agent protocol.

## Pushing objects (operator → target)

Before applying a commit, the operator must ensure the target's store has all
the objects that commit references. Rather than opening a second SSH connection
for `git push`, we send a packfile over the existing agent session:

 1. The operator uses libgit2's `PackBuilder` to create a packfile containing
    the target commit and all objects it references (trees, blobs).
 2. The packfile is base64-encoded and sent as a `ReceivePack` request.
 3. The agent decodes the base64 and writes the pack into its ODB using
    libgit2's `OdbPackwriter`.

The pack is built once and reused for all hosts that need it.

## Fetching objects (target → operator)

When a deploy lock reports that the target's `current` ref points to a commit
the operator doesn't have (because someone else deployed in the meantime), the
operator needs those objects to be able to diff against them in the next plan.

The agent builds a packfile from its side and sends it back in a `SendPack`
message, in response to a `RequestObjects` request from the operator. The
operator writes the pack into its local store, then updates the tracking ref.

As a fallback (e.g. if the session is already closed), the operator can also
run `git fetch` against the target, which requires Git on the target.
