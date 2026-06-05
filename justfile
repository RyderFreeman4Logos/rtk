# Justfile for rtk (Rust Token Killer)
# Adapted from ../cli-sub-agent/justfile for rtk's single-package + release-please setup.
# ⚠️ AI AGENT: Do NOT bypass pre-commit via `git commit -n` / `--no-verify`. Fix the actual code.
#
# Key differences from cli-sub-agent:
#   - rtk is a SINGLE package (`rtk`), not a workspace → no `--workspace`, no `weave`.
#   - rtk versions via release-please → manual `bump-patch` is unnecessary;
#     `check-version-bumped` is a deliberate no-op (see recipe).
#   - Lint/test commands mirror rtk's CI (.github/workflows/ci.yml) exactly so that
#     `just` locally == CI: `cargo fmt --all -- --check`, `cargo clippy --all-targets`,
#     `cargo test --all`. (cli-sub-agent uses `-D warnings` + nextest; rtk does not.)

set shell := ["bash", "-c"]
# Keep Just's transient scripts inside the repo so sandboxed commit paths
# do not depend on a writable XDG runtime dir such as /run/user/$UID.
set tempdir := "."
# Automatically load .env file if present
set dotenv-load := true

# Calculate repo root
_repo_root := `git rev-parse --show-toplevel`
# Just already executes repository-controlled code, so trust this checkout's
# mise config and avoid interactive trust prompts on sandboxed commit paths.
export MISE_TRUSTED_CONFIG_PATHS := _repo_root

# Default recipe: full local gate.
default: pre-commit

# ==============================================================================
# 🚀 Core Workflow
# ==============================================================================

# Fast pre-commit: artifact guard, formatting, and linting only (no tests).
# Tests run in the pre-push hook instead, keeping commits snappy.
pre-commit-fast:
    just check-generated-artifacts
    just fmt
    just clippy

# Full local gate: fast checks plus the test suite.
pre-commit:
    just pre-commit-fast
    just test

# ==============================================================================
# 🎨 Formatting & Linting
# ==============================================================================

# Format code and re-stage only the .rs files that were already staged before fmt.
# Aborts first if any staged Rust file also has unstaged hunks (avoids surprising
# the user by staging hunks they deliberately left out).
fmt:
    #!/usr/bin/env bash
    set -euo pipefail
    staged_rs=()
    while IFS= read -r -d '' path; do
        staged_rs+=("$path")
    done < <(git diff --cached --name-only -z -- '*.rs')
    unstaged_rs=()
    while IFS= read -r -d '' path; do
        unstaged_rs+=("$path")
    done < <(git diff --name-only -z -- '*.rs')
    partial=()
    for staged in "${staged_rs[@]:-}"; do
        [ -z "$staged" ] && continue
        for unstaged in "${unstaged_rs[@]:-}"; do
            if [[ "$staged" == "$unstaged" ]]; then
                partial+=("$staged")
                break
            fi
        done
    done
    if (( ${#partial[@]} > 0 )); then
        printf 'just fmt: refusing to format -- these Rust files are partially staged (mixed staged/unstaged hunks); stage or stash the remaining hunks first:\n' >&2
        printf '  %q\n' "${partial[@]}" >&2
        exit 1
    fi
    if (( ${#staged_rs[@]} == 0 )); then
        exit 0
    fi
    cargo fmt --all
    printf '%s\0' "${staged_rs[@]}" | xargs -0 git add --

# Check formatting without modifying files (mirrors CI: `cargo fmt --all -- --check`).
fmt-check:
    cargo fmt --all -- --check

# Run clippy across all targets (mirrors CI: `cargo clippy --all-targets`).
clippy:
    cargo clippy --all-targets

# Alias used by language-agnostic tooling (dev2merge/mktsk `just lint`).
lint: clippy

# ==============================================================================
# 🧪 Testing
# ==============================================================================

# Run the full test suite (mirrors CI: `cargo test --all`).
test:
    cargo test --all

# Run a single test by name. Usage: just test-f rg_grep_rewrite
test-f pattern:
    cargo test --all {{pattern}}

# ==============================================================================
# 🔒 Guards
# ==============================================================================

# Fail if generated or scratch artifacts are staged for commit (rule 062).
# Allows deletions so cleanup commits can remove previously tracked artifacts.
check-generated-artifacts:
    #!/usr/bin/env bash
    set -euo pipefail
    blocked="$(git diff --cached --name-only --diff-filter=ACMR \
        | grep -E '^(target/|\.tmp/|.*\.log$|.*_output\.[^/]*$|diff\.txt$)' || true)"
    if [ -n "${blocked}" ]; then
        echo "ERROR: Generated or scratch artifacts are staged:" >&2
        printf '  %s\n' ${blocked} >&2
        echo "Remove these from the commit and keep them ignored." >&2
        exit 1
    fi

# Version is managed by release-please (see release-please-config.json), which
# bumps Cargo.toml + CHANGELOG.md on merge to main. Manual bumps on feature
# branches are therefore unnecessary; this recipe is a deliberate no-op so that
# generic pipelines (dev2merge Step 9) treat "version already handled" as success.
check-version-bumped:
    @echo "rtk versions via release-please; no manual bump needed. (no-op)"

# ==============================================================================
# 🪝 Git Hooks
# ==============================================================================

# Install git hooks via lefthook. Safe to run multiple times.
install-hooks:
    @git config --unset core.hooksPath 2>/dev/null || true
    lefthook install
    @echo "Lefthook hooks installed."
