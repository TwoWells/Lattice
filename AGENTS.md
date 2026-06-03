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
- **Required tools:** cargo-deny, cargo-machete, cargo-nextest, cargo-mutants, cargo-fuzz.
- **Install tools:** `cargo binstall cargo-deny cargo-machete cargo-nextest cargo-mutants cargo-fuzz`
- **Nightly toolchain:** only `cargo-fuzz` needs it (for the libFuzzer/ASAN
  build); everything else runs on the pinned stable toolchain.

## Development Commands

- **Check (full):** `make check` — format, lint, deny, machete, and test in one pass.
- **Test (all):** `make test`
- **Test (filtered):** `make test T=<filter>`
- **Test (repeat):** `make test T=<filter> N=<count>`

## Fuzzing

Coverage-guided fuzz targets (cargo-fuzz / libFuzzer) live in `fuzz/` and are
**not** part of `make test` — they run until stopped. They require the nightly
toolchain; `make fuzz` forces it.

- **Smoke (all targets):** `make fuzz` — runs each target sequentially for
  `FUZZ_TIME` seconds (default 60).
- **Single target:** `make fuzz T=fuzz_yaml`
- **Parallel soak:** `make soak FUZZ_TIME=3600` — runs all targets *at once*
  (one process each), so a 1 h/target soak takes ~1 h of wall-clock instead of
  ~7 h. Needs ≥8 cores and ~3-4 GB RAM; per-target logs in `fuzz/soak-*.log`.
- **Targets** (one per parser entry point): `fuzz_parse_tree`, `fuzz_yaml`,
  `fuzz_toml`, `fuzz_json`, `fuzz_full`, `fuzz_tokenize_tag`, `fuzz_inlines`.

**The assertions are the product, the fuzzer is the input generator.** Each
target embeds the same invariants as the property suite — tree
well-formedness, content fidelity, and LSP position round-trip — factored into
`src/invariants.rs` and called by both `src/property_tests.rs` and the fuzz
targets, so the two suites cannot drift. A target that only catches panics is
blind to silent wrong-output bugs (the largest class); never weaken a target to
"no panic" only. If an invariant flags a *correct* parse (e.g. a legitimately
decoded escape), refine the invariant precisely — do not broaden it into
uselessness.

- **Seed corpora:** `fuzz/corpus/<target>/` — curated seeds (committed; they
  carry a file extension). libFuzzer's discovered inputs land here too but are
  git-ignored. Seeds span each parser's syntax surface **and** the encoding
  axis (mixed line endings, BOM, multi-byte, zero-width).
- **A finding is a real, reproducible bug** (a shrunk counterexample), never a
  flake. Fix the parser (or, if the invariant was over-strict, the invariant),
  add the reproducing input to `fuzz/corpus/<target>/`, and add a deterministic
  unit test to the parser's `#[cfg(test)]` module so the fix is permanent
  without depending on the fuzzer.

## Release Workflow

- **Patch Release:** `make release-patch`
- **Minor Release:** `make release-minor`
- **Major Release:** `make release-major`
- **Custom Version:** `make release V=x.y.z`

Release runs: check → commit → tag.
