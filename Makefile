# Lattice Makefile
# Usage:
#   make setup           # configure hooks + check tools (first time)
#   make check           # fmt, lint, deny, machete, test
#   make release-patch   # 0.1.0 -> 0.1.1
#   make release-minor   # 0.1.0 -> 0.2.0
#   make release-major   # 0.1.0 -> 1.0.0
#   make release V=0.2.0 # explicit version

.PHONY: build-release check deny fuzz soak machete metadata mutants setup setup-hooks setup-tools test release release-patch release-minor release-major publish tag-current

# Get current version from Cargo.toml
CURRENT_VERSION := $(shell grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Required cargo tools. cargo-fuzz additionally needs the nightly toolchain
# (see `make fuzz`); it is only used for fuzzing, like cargo-mutants is for
# mutation testing.
CARGO_TOOLS := cargo-deny cargo-machete cargo-nextest cargo-mutants cargo-fuzz

# Hard per-process address-space cap (KiB) applied to test runs, so a runaway
# allocation (e.g. a pathological property-test input) aborts that single
# process at the limit instead of exhausting system RAM. Override with
# MEMLIMIT_KB=<kib>, or MEMLIMIT_KB=unlimited to disable.
MEMLIMIT_KB ?= 8388608

# --- Setup ---

# One-time setup: configure hooks and check tools
setup: setup-hooks setup-tools

# Configure git hooks (explicit opt-in, not run by check)
setup-hooks:
	@current=$$(git config core.hooksPath 2>/dev/null); \
	 if [ "$$current" = ".githooks" ]; then \
	   echo "hooks: already configured"; \
	 else \
	   git config core.hooksPath .githooks; \
	   echo "hooks: configured .githooks"; \
	 fi

# Report cargo tool status
setup-tools:
	@missing=0; \
	 for tool in $(CARGO_TOOLS); do \
	   if ! command -v $$tool >/dev/null 2>&1 && ! cargo --list 2>/dev/null | grep -qw "$${tool#cargo-}"; then \
	     echo "  missing: $$tool"; \
	     missing=1; \
	   else \
	     echo "  ok: $$tool"; \
	   fi; \
	 done; \
	 if [ $$missing -eq 1 ]; then \
	   echo ""; \
	   echo "Install missing tools with:"; \
	   echo "  cargo binstall cargo-deny cargo-machete cargo-nextest cargo-mutants cargo-fuzz"; \
	   echo ""; \
	   echo "Or with cargo install (slower, builds from source):"; \
	   echo "  cargo install cargo-deny cargo-machete cargo-nextest cargo-mutants cargo-fuzz"; \
	 else \
	   echo "All tools present."; \
	 fi

# --- Check ---

build-release:
	@cargo build --release

check: setup-tools
	@PINNED=$$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml); \
	 LATEST=$$(rustup run stable rustc --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+' | head -1); \
	 if [ -n "$$LATEST" ] && [ "$$PINNED" != "$$LATEST" ]; then \
	   printf '\033[33mNote: rust-toolchain.toml pins %s, latest stable is %s\033[0m\n' "$$PINNED" "$$LATEST"; \
	 fi
	@cargo update --quiet
	@cargo fmt -- -l | sed 's/^/fmt: formatted /'
	@cargo clippy --tests --quiet -- -D warnings
	@tries=0; while true; do \
	   cargo deny --log-level error check; rc=$$?; \
	   if [ $$rc -eq 0 ]; then break; \
	   elif [ $$rc -ne 139 ]; then exit $$rc; \
	   else \
	     tries=$$((tries + 1)); \
	     if [ $$tries -ge 5 ]; then echo "cargo-deny segfaulted 5 times, giving up"; exit 139; fi; \
	     echo "cargo-deny segfaulted (EmbarkStudios/cargo-deny#855), retry $$tries/5..."; \
	   fi; \
	 done
	@cargo machete --skip-target-dir
	@if [ "$(MEMLIMIT_KB)" != unlimited ]; then ulimit -v $(MEMLIMIT_KB); fi; \
	 cargo nextest run --no-fail-fast --no-tests=pass --status-level fail --final-status-level fail --cargo-quiet --show-progress only

deny:
	@cargo deny --log-level error check

machete:
	@cargo machete --skip-target-dir

# Emit `cargo metadata` JSON WITHOUT mutating the lockfile: `--locked` makes
# cargo error rather than update if Cargo.lock is out of date, so this both
# verifies the lock is consistent (`make metadata >/dev/null`) and feeds tools
# like jq. Unlike `make check` (which runs `cargo update`), it never bumps deps.
metadata:
	@cargo metadata --locked --format-version 1

# --- Test ---

mutants:
	@cargo mutants --timeout 60

# --- Fuzzing (ticket 22) ---
#
# cargo-fuzz targets live in fuzz/. They require the nightly toolchain (for the
# libFuzzer/ASAN instrumentation) and are NOT part of `make test` — they run
# until stopped. `make fuzz` runs each target for a bounded duration as a local
# smoke test; a dedicated CI job runs them far longer on a schedule. Crash
# artifacts land in fuzz/artifacts/<target>/ and are checked in as regressions.
#
#   make fuzz                  # all targets sequentially, FUZZ_TIME seconds each
#   make fuzz T=fuzz_yaml      # a single target
#   make fuzz FUZZ_TIME=600    # longer per-target run (10 min)
#   make soak FUZZ_TIME=3600   # all targets IN PARALLEL, 1 h each (~1 h wall)
FUZZ_TARGETS := fuzz_parse_tree fuzz_yaml fuzz_toml fuzz_json fuzz_full fuzz_tokenize_tag fuzz_inlines fuzz_edits
FUZZ_TIME ?= 60

# A prebuilt (musl) cargo-fuzz binary defaults `--target` to its own musl
# platform, which has no installed std and is ASAN-incompatible. Pin the host
# gnu target instead; override FUZZ_TRIPLE if your host differs.
FUZZ_TRIPLE ?= x86_64-unknown-linux-gnu

fuzz:
	@command -v cargo-fuzz >/dev/null 2>&1 || { \
	  echo "cargo-fuzz not found. Install with: cargo binstall cargo-fuzz"; exit 1; }
	@export RUSTUP_TOOLCHAIN=nightly; \
	 run_one() { \
	   echo "=== fuzzing $$1 (max $(FUZZ_TIME)s) ==="; \
	   cargo fuzz run "$$1" --target $(FUZZ_TRIPLE) -- -max_total_time=$(FUZZ_TIME); \
	 }; \
	 if [ -n "$(T)" ]; then \
	   run_one "$(T)"; \
	 else \
	   for t in $(FUZZ_TARGETS); do run_one "$$t" || exit $$?; done; \
	 fi

# Soak every target IN PARALLEL for FUZZ_TIME seconds each (one process per
# target), so `make soak FUZZ_TIME=3600` meets a 1 h/target bar in ~1 h of
# wall-clock instead of ~8 h. Each target is single-threaded, so on a machine
# with >= 9 cores (8 targets + headroom) there is no contention. Per-target
# output goes to fuzz/soak-<target>.log; a non-zero exit means at least one
# target crashed (its reproducer is under fuzz/artifacts/<target>/).
soak:
	@command -v cargo-fuzz >/dev/null 2>&1 || { \
	  echo "cargo-fuzz not found. Install with: cargo binstall cargo-fuzz"; exit 1; }
	@export RUSTUP_TOOLCHAIN=nightly; \
	 echo "Building fuzz targets..."; \
	 cargo fuzz build --target $(FUZZ_TRIPLE) || exit 1; \
	 echo "Soaking $(words $(FUZZ_TARGETS)) targets in parallel for $(FUZZ_TIME)s each..."; \
	 pids=""; \
	 for t in $(FUZZ_TARGETS); do \
	   cargo fuzz run "$$t" --target $(FUZZ_TRIPLE) -- -max_total_time=$(FUZZ_TIME) \
	     > fuzz/soak-$$t.log 2>&1 & \
	   pids="$$pids $$!:$$t"; \
	 done; \
	 fail=0; \
	 for pt in $$pids; do \
	   pid=$${pt%%:*}; t=$${pt#*:}; \
	   if wait $$pid; then echo "  ok:   $$t"; \
	   else echo "  FAIL: $$t (see fuzz/soak-$$t.log and fuzz/artifacts/$$t/)"; fail=1; fi; \
	 done; \
	 if [ $$fail -eq 0 ]; then echo "Soak clean: all targets survived $(FUZZ_TIME)s."; fi; \
	 exit $$fail

# Run tests. Pass T= to filter, N= to repeat, PROFILE= to select a nextest
# profile (e.g. PROFILE=hardening for extended PROPTEST_CASES / fork runs).
CLEAN_T = $(subst \,,$(subst !,,$(T)))
test:
	@if [ "$(MEMLIMIT_KB)" != unlimited ]; then ulimit -v $(MEMLIMIT_KB); fi; \
	 cargo nextest run $(if $(PROFILE),--profile $(PROFILE),) --status-level fail --final-status-level slow --cargo-quiet $(if $(N),--stress-count $(N),) $(if $(T),$(if $(findstring !,$(T)),-E 'not test($(CLEAN_T))',-E 'test($(T))'),)

# --- Release ---

pre-release-check:
	@echo "Checking release prerequisites..."
	@if [ -n "$$(git status --porcelain)" ]; then \
		echo "Error: Working tree is not clean. Commit or stash changes first."; \
		exit 1; \
	fi
	@if [ "$$(git branch --show-current)" != "main" ]; then \
		echo "Error: Not on main branch."; \
		exit 1; \
	fi
	@git fetch origin main --quiet
	@if [ "$$(git rev-parse HEAD)" != "$$(git rev-parse origin/main)" ]; then \
		echo "Error: Local main is not up to date with origin/main."; \
		exit 1; \
	fi
	@echo "Prerequisites OK."

bump-version:
	@if [ -z "$(V)" ]; then \
		echo "Error: Version not specified. Use V=x.y.z"; \
		exit 1; \
	fi
	@echo "Bumping version: $(CURRENT_VERSION) -> $(V)"
	@sed -i 's/^version = "$(CURRENT_VERSION)"/version = "$(V)"/' Cargo.toml
	@cargo check --quiet
	@echo "Version bumped to $(V)"

next-patch:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1"."$$2"."$$3+1}'))

next-minor:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1"."$$2+1".0"}'))

next-major:
	$(eval V := $(shell echo $(CURRENT_VERSION) | awk -F. '{print $$1+1".0.0"}'))

release: pre-release-check
	@if [ -z "$(V)" ]; then \
		echo "Error: Version not specified. Use 'make release V=x.y.z' or 'make release-patch'"; \
		exit 1; \
	fi
	@cargo update --quiet
	@$(MAKE) bump-version V=$(V)
	@if ! $(MAKE) check; then \
		echo "Checks failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock; \
		exit 1; \
	fi
	@git add Cargo.toml Cargo.lock
	@if ! git commit -m "chore: Bump version to $(V)"; then \
		echo "Commit failed. Rolling back version bump..."; \
		git checkout HEAD -- Cargo.toml Cargo.lock; \
		exit 1; \
	fi
	@git tag -a "v$(V)" -m "Release v$(V)"
	@echo ""
	@echo "Release v$(V) prepared locally."
	@echo "Run 'make publish' to push and create the release."

release-patch: pre-release-check next-patch
	@$(MAKE) release V=$(V)

release-minor: pre-release-check next-minor
	@$(MAKE) release V=$(V)

release-major: pre-release-check next-major
	@$(MAKE) release V=$(V)

publish:
	@echo "Pushing to origin..."
	@git push && git push --tags
	@echo ""
	@echo "Release v$(CURRENT_VERSION) pushed."

tag-current:
	@if git rev-parse "v$(CURRENT_VERSION)" >/dev/null 2>&1; then \
		echo "Tag v$(CURRENT_VERSION) already exists."; \
		exit 1; \
	fi
	@echo "Creating tag v$(CURRENT_VERSION) for current version..."
	@git tag -a "v$(CURRENT_VERSION)" -m "Release v$(CURRENT_VERSION)"
	@echo "Tag created. Run 'make publish' to push and release."

version:
	@echo "Current version: $(CURRENT_VERSION)"
	@echo "Latest tag:      $$(git describe --tags --abbrev=0 2>/dev/null || echo 'none')"
