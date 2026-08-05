# ADR 0001: Architecture and lifecycle contract

## Decision

`ewtm` remains one binary package with explicit `cli`, `tui`, `application`,
`domain`, and `infrastructure` modules. The domain contains portable concepts
and rules only: it does not import CLI/TUI, filesystem, terminal, or subprocess
APIs. Application coordinates use cases; infrastructure owns Git and external
processes; CLI and TUI are outer adapters over application use cases.

## Operation semantics and safety guards

- **create** plans and then creates one linked worktree at the requested path,
  refusing an occupied path or branch and never overwriting user files.
- **remove** removes only a worktree registered to the current repository.
  Primary checkouts are always protected. Dirty worktree removal, local branch
  deletion, forced local branch deletion, and remote branch deletion each have
  separate explicit consents/policies; no generic `--force` exists and no
  consent implies another one. Branch retention is the default, so removal
  does not delete a local branch unless separately requested.
- **sync** performs one fetch of the configured remotes, then fast-forwards
  only clean worktrees with a configured upstream. Detached, no-upstream, and
  diverged worktrees are skipped or refused. It never automatically stashes,
  rebases, resets, or resolves conflicts.
- **rollback** is safe compensation selected by an explicit operation ID, not
  “the last operation”. It may compensate only steps created by ewtm whose
  recorded preconditions still hold. Dirty-data loss, remote branch deletion,
  provider/external side effects, and changed object IDs are irreversible or
  `needs_attention`; they receive no automatic reversal.

Every mutation checks the primary-checkout guard, branch-in-another-worktree
guard, dirty/untracked guard, ongoing-operation guard, unmerged/unpushed/ahead
guard, and expected-object-ID guard as applicable. A failed guard refuses the
operation rather than guessing or silently repairing state.

## Providers

Providers are optional GitHub/GitLab adapters behind an application boundary.
They use `gh`/`glab`, never store credentials, and return typed results. A
provider publish step after successful local creation leaves the local result
in place if it fails. Provider publish steps are independently retryable and
resumable; they do not claim to roll back local or external side effects.

These contracts are the M0 boundary; implementations arrive in later milestones.
