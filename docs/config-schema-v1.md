# Configuration schema v1

This is the deterministic configuration contract for M1/M3.

## M2A1 planning additions

`[create]` accepts `default_base`, `slug_max_bytes`, `worktree_root`, and
`directory_prefix`. A relative `worktree_root` is resolved against the parent
of the primary worktree; the default root is that same parent. The default
directory prefix is `<repository-name>-`; branch slashes become hyphens. A
caller-supplied relative `--path` is resolved against the invocation directory.

`[git].remote` is a validated non-empty name and defaults to `origin`.
File rules explicitly use `match_mode = "path"` or `"glob"`; glob destinations
are directory roots. Ordinary legacy paths use `path`, while the imported
implicit environment rule uses `glob`. Exclusions are relative patterns and a
basename exclusion applies at every matched level. Rule order is stable by
name and manifest path; overlapping rules are rejected by the planner.
Copy/copy-tree and symlink rules fail on conflict; relink requires
`replace_symlink_only`. `enabled`, `sensitive`, and `confirm` remain
independent controls. Only selected enabled `post_create` tasks are planned.
Each sensitive rule and selected task has an exact consent id:
`file-rule:<name>` or `task:<name>`.

Lifecycle plans persist only reversible `StoredPath` values. Worktree guards
carry their exact path and object ID; removal is irreversible and has no
automatic recreation compensation. Removal consent IDs are
`remove:dirty`, `remove:local-branch`, `remove:force-local-branch`, and
`remove:remote:<remote>/<branch>`.

## Locations and precedence

Files are read, when present, in this order (lowest to highest priority):

1. defaults
2. user: `~/.config/ewtm/config.toml`
3. version-controlled project: `.ewtm.toml`
4. repository-local: `<git-common-dir>/ewtm/config.toml`
5. CLI flags and arguments

Later values override earlier values. The project file is version-controlled
with the project checkout. The repository-local file is anchored at Git's
common directory, not at a linked worktree's administrative directory.

## Named entries and merging

`file_rules`, `tasks`, and `hooks` are named TOML tables, never anonymous
arrays. The table name is the stable key, for example
`[tasks.test]` has stable key `test`. A higher layer replaces the complete
entry with the same key; fields are not implicitly merged. An entry absent
from a higher layer remains unchanged. `enabled = false` disables an entry
without deleting it; `delete = true` removes that named entry from the
effective configuration. `delete` entries may contain no other operational
fields. CLI selection can additionally disable a named entry for one run.

The effective `create` section uses `slug_max_bytes` (default `60`, minimum
`8`), optional `worktree_root`, and optional `directory_prefix`. A
`worktree_root` may be absolute or sibling-relative; final containment checks
belong to the M2 planner. Effective rules always have relative `source` and
`destination`, a `source_root` of `current_worktree` or `primary_worktree`,
optional `ignored_only` and `excludes`, and conflict policy `fail` or
`replace_symlink_only`.

## Paths, rules, and environment

Relative paths resolve against the root of the worktree/repository where the
effective configuration is applied. ewtm canonicalizes existing roots and
checks that resolved paths remain below the intended root. It does not follow
unexpected symlinks while validating or applying paths; symlink targets must
be explicit and remain within the permitted root.

File rules use one of `copy`, `copy_tree`, `symlink`, or `relink`. The default
`on_conflict` policy is `fail`; no rule overwrites an existing path silently.
`sensitive = true` marks data requiring explicit confirmation, while
`confirm = true` requires confirmation before applying the rule. Sensitive
environment variables are disabled by default and may only be exposed through
an explicit task environment allowlist.

Tasks and lifecycle hooks are declarative argv tasks. Each has `phase`, `argv`
(a non-empty argument array), optional `cwd`, `required`, and an
`environment_allowlist`. There are no shell command strings, shell expansion,
Bash, or executable configuration files. Parsing TOML never executes code;
approved lifecycle hooks may later execute only their declared argv.

Trust approval is stored only in repository-local state under
`<git-common-dir>/ewtm/`. It is bound to the repository identity, the exact
effective-config digest, and the exact command digest (including argv, cwd,
and allowed environment). Any change to those inputs revokes approval.
Credentials are forbidden in configuration and trust state.

## Legacy `.worktreerc` import

`config import` reads the source as a non-executing assignment subset. It
accepts comments, quoted strings, strict decimal `0`/`1` flags, and Bash arrays
using `(...)`; legacy path arrays also accept safe unquoted path words. Shell
expansion, backslash escapes, operators, redirects, functions, and control
constructs are rejected. Symlink/relink settings map to disabled sensitive
primary-worktree rules, and install/build settings map to disabled
`post_create` argv tasks. CodeGraph and implicit `.env` copying are disabled
and returned as source-located diagnostics. Structural errors fail the import;
safe assignments may still be returned with warnings.

## Compact example

```toml
schema = 1

[file_rules.app_config]
kind = "copy"
source = "config/example.toml"
destination = "config/local.toml"
on_conflict = "fail"
sensitive = true
confirm = true
enabled = true

[file_rules.assets]
kind = "copy_tree"
source = "assets"
destination = "assets"

[tasks.test]
phase = "manual"
argv = ["cargo", "test"]
cwd = "."
required = true
environment_allowlist = []

[hooks.after_create]
phase = "post_create"
argv = ["cargo", "fmt", "--all"]
cwd = "."
required = false
environment_allowlist = ["RUST_LOG"]
enabled = true
```
