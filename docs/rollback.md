# Rolling back

TODO(ruuda): explain that the config tree is the source of truth, so rollback
is `git revert` (or restoring a previous tree) followed by `deptool deploy`.

## Reverting a deploy

TODO(ruuda): worked example.

## Auto-rollback during deploy

TODO(ruuda): cross-link to [deployment phases](deployment_phases.md#rollback).

## Limitations

TODO(ruuda): when rollback is not possible (plans that create new symlinks).
