# Transport

Deptool transfers Git objects between the operator machine and target hosts
without requiring Git to be installed on the target. All data flows over the
same SSH session used for the agent protocol.

## Pushing objects (operator -> target)

Before applying a commit, the operator must ensure the target's store has all
the objects that commit references. Rather than opening a second SSH connection
for `git push`, we build a packfile using libgit2 containing only the objects
the target doesn't already have, base64-encode it, and send it over the
existing agent session. The agent writes the pack into its object database.

## Fetching objects (target -> operator)

When a deploy lock reports that the target's `current` ref points to a commit
the operator doesn't have (because someone else deployed in the meantime), the
operator needs those objects to be able to diff against them in the next plan.

The operator sends a request over the still-open session, and the agent builds
a packfile from its side and sends it back. The operator writes the pack into
its local store, then updates its tracking ref.
