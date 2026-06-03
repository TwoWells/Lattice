# Lattice Agent Context

This file serves as the single point of truth for AI agents working on the Lattice project.

## Project

- **Goal:** Markdown predicate linter and backlink reconciler, shipped as an LSP server.
- **Repository:** `TwoWells/Lattice` on GitHub.
- **License:** AGPL-3.0-or-later with commercial license option.

Single crate. The `lattice` binary serves as both the LSP server and the CLI (`lattice lint`).

## Coding Standards

- **Edition:** Rust 2024.
- **Safety:** `unsafe` code is strictly forbidden (`forbid(unsafe_code)`).
- **Error Handling:** Use `anyhow` for application logic and `thiserror` for library errors.
- **Strict Denials:** Do NOT use `unwrap()`, `panic!()`, `todo!()`, `unimplemented!()`, `dbg!()`, `println!()`, or `eprintln!()`. Use proper error handling and the `tracing` crate for logging. `expect()` is denied in production code but allowed in `#[cfg(test)]` modules — prefer `expect("reason")` over `anyhow` workarounds in tests.
- **Assertions:** All `assert!()`, `assert_eq!()`, and `assert_ne!()` calls must include a message explaining what failed.
- **Imports:** No wildcard imports (`use crate::*`).
- **Formatting:** Code must be formatted with `rustfmt`.
- **Linting:** Must pass `cargo clippy` with `pedantic`, `nursery`, and `cargo` groups enabled. Every `#[allow(...)]` must include a `reason` string.

## Commit Convention

- **Format:** [Conventional Commits](https://www.conventionalcommits.org/) (enforced by commit-msg hook).
- **Pattern:** `type(scope): description` or `type: description`
- **Types:** `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`
- **Breaking changes:** append `!` before colon, e.g. `feat(config)!: change predicate format`
- **Examples:**
  - `feat(backlinks): add frontmatter consistency check`
  - `fix(parser): correct title text extraction`
  - `test(lint): add bare path detection tests`
  - `chore: bump version to 0.2.0`

## Quality Standards

- **License Compliance:** All new dependencies MUST have permissive licenses (MIT, Apache-2.0, etc.) as specified in `@./deny.toml`. Lattice is dual-licensed under AGPL-3.0-or-later and a commercial license.
- **Copyright Headers:** Every `.rs` file must start with the SPDX header (enforced by pre-commit hook):
  ```
  // SPDX-License-Identifier: AGPL-3.0-or-later
  // Copyright (C) 2026 Two Wells <contact@twowells.dev>
  ```
- **Documentation:** All public APIs must have documentation comments.
- **Testing:** All new features must include tests.
- **Property tests are not flaky.** The `proptest` suite (`src/property_tests.rs`) draws fresh random inputs each run, but every discovered failure is saved to `proptest-regressions/` and replayed first on subsequent runs, and `nextest` retries are disabled. A property-test failure is therefore a real, reproducible bug with a shrunk counterexample — fix the parser. **Never** re-run to make it pass or treat it as a flake. `make test` also kills any test that hangs past 120s (see `.config/nextest.toml`).

## Setup

- **First time:** `make setup` — configures git hooks and checks for required cargo tools.
- **Required tools:** cargo-deny, cargo-machete, cargo-nextest, cargo-mutants.
- **Install tools:** `cargo binstall cargo-deny cargo-machete cargo-nextest cargo-mutants`

## Development Commands

- **Check (full):** `make check` — format, lint, deny, machete, and test in one pass.
- **Test (all):** `make test`
- **Test (filtered):** `make test T=<filter>`
- **Test (repeat):** `make test T=<filter> N=<count>`

## Release Workflow

- **Patch Release:** `make release-patch`
- **Minor Release:** `make release-minor`
- **Major Release:** `make release-major`
- **Custom Version:** `make release V=x.y.z`

Release runs: check → commit → tag.
