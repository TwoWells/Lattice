# Both-bases resolution and excluded shapes (issue 028)

A `..`-relative reference `../claude_code/PostToolUse.md` and a
repo-root-relative citation `tickets/acquire/DESIGN.md` written in prose.

Excluded shapes that are not workspace paths: `~/Projects/Catenary/AGENTS.md`
(home-relative), `<name>/SKILL.md` (placeholder), and `NN_*.md` (glob).

A bare `../sibling.md` and a bare `tickets/acquire/DESIGN.md` for the
tree-level scanner, plus a bare glob docs/NN_*.md and a bare placeholder
<name>/note.md.

| # | Ref |
|---|-----|
| 1 | `../claude_code/Hook.md#anchor` |
| 2 | `~/out/of/repo.md` |
