# Releasing Lattice

The maintainer runbook. Four steps, in this order — the order is the point,
because the last step cannot be undone.

## Release order

1. **Push `main`.** `make release-*` refuses to run unless local `main` equals
   `origin/main`, and says which way they differ. The history a tag names must
   already be public: a tag that points at commits only you have is a release
   nobody else can build.

2. **Wait for CI to go green** — `gh run watch`. This is not what protects the
   crate: `release.yml` runs its own `check` job and gates the crates.io
   publish on it, so a broken tree cannot publish. What the wait protects is
   the tag. Cut a release on a red tree and you get a public `vX.Y.Z` whose
   pipeline went red, and the cleanup is public too — delete the tag remote and
   local (`git push --delete origin vX.Y.Z`, `git tag -d vX.Y.Z`), fix, re-cut.

3. **Cut the release** — `make release-minor` (or `release-patch`,
   `release-major`, `release V=x.y.z`). Bumps `Cargo.toml`, runs the full
   `make check` against the bumped tree, commits `chore: Bump version to
   X.Y.Z`, and tags `vX.Y.Z`. All of it is local; a failed check rolls the bump
   back and nothing is committed.

4. **Publish** — `make publish` pushes the bump commit and the tag.

Steps 1–3 are undoable with `git reset` and `git tag -d`. Step 4 is not.

## The irreversible boundary

The tag push in step 4 triggers `release.yml`, which runs, in order:

- **verify** — the tag matches `Cargo.toml`'s version.
- **check** — `make check` on the tagged tree; gates everything below it.
- **build** — cross-platform binaries (Linux, Windows, macOS).
- **publish-crate** — crates.io via Trusted Publishing (OIDC, no stored token).
  **This is irreversible.** A published version can be yanked, never replaced
  and never deleted; the next fix is a new version number.
- **release** — the GitHub Release, with the binaries attached and attested.
- **notify-distribution** — dispatches the packaging repos.

## Local deployment

`make install` runs `cargo install --path . --locked`: it puts *this
checkout's* binary on PATH with the reviewed lockfile's dependency resolution,
not the registry's latest. Then verify and restart:

```sh
lattice --version   # prints the build's commit hash — that is what it is for
```

Check the hash every time, and restart your editor's LSP sessions afterwards —
a session started before the install keeps the old process alive, so the
restart is part of the deployment rather than an afterthought.

The failure mode this prevents is a stale PATH binary silently judging current
repos. Nothing errors: the diagnostics look real, they are just an older
build's, and the time goes into debugging the wrong layer.

## Lockfile stance

A release ships exactly the reviewed `Cargo.lock`. Nothing in the release path
updates dependencies — every cargo invocation in `make check` that accepts
`--locked` takes it, so a lockfile disagreeing with `Cargo.toml` fails the
check instead of being rewritten mid-run.

A fresh-deps release is two deliberate acts, not one: `make update` (the only
local target that rewrites the lockfile), review and commit the diff, then
`make release-*`.

## One-time setup

crates.io publishing uses Trusted Publishing, so no long-lived token is stored
in GitHub secrets or reaches the runner. Before the first successful publish:

- crates.io → the crate's Settings → Trusted Publishing → add repository
  `TwoWells/Lattice` and workflow file `release.yml`.

If you register an Environment there, it must match the `environment: release`
named by `release.yml`'s `publish-crate` job.
