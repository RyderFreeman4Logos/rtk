#!/usr/bin/env bash
# Branch protection: block work on protected branches.
#
# Wired into BOTH pre-commit and pre-push (see lefthook.yml):
#   - pre-commit: blocks `git commit` while HEAD is on main/dev/master.
#   - pre-push:   backstop that blocks `git push` while HEAD is on a protected
#                 branch — catches commits that reached a protected branch
#                 WITHOUT pre-commit running (local merge/cherry-pick, or
#                 commits made before hooks were installed). (AGENTS rule 056)
#
# SCOPE / KNOWN LIMITATION (intentional — documented for reviewers):
#   This guard keys off the CURRENT branch (`git symbolic-ref --short HEAD`).
#   It does NOT catch an explicit-refspec push to a protected REMOTE ref from a
#   non-protected checkout, e.g. `git push origin feat/x:main`. That destination
#   ref is delivered to a pre-push hook ONLY on git's stdin — and lefthook 2.1.9
#   redirects each command's stdin to empty and exposes no ref/refspec env var
#   (empirically verified on 2.1.9), so a lefthook-managed command cannot see
#   the push destination. Reading git's stdin would require a raw, unmanaged
#   pre-push hook, which conflicts with lefthook owning + regenerating
#   .git/hooks/pre-push on every `lefthook install`.
#   The authoritative defense against pushing to a protected branch is therefore
#   GitHub SERVER-SIDE branch protection (require PRs / restrict who can push);
#   this local hook is best-effort early feedback for the common accident, not a
#   security boundary (it is bypassable via --no-verify or an uninstalled hook).
#   Copied/adapted from ../cli-sub-agent/scripts/hooks/branch-protection.sh.
set -euo pipefail

branch=$(git symbolic-ref --short HEAD 2>/dev/null) || exit 0
[ -z "$branch" ] && exit 0  # detached HEAD

PROTECTED="main dev master"

for pb in $PROTECTED; do
  if [ "$branch" = "$pb" ]; then
    echo ""
    echo "BLOCKED: Cannot commit or push directly to '$branch'."
    echo ""
    echo "Create a feature branch first:"
    echo "  git checkout -b feat/<description>"
    echo "  git checkout -b fix/<description>"
    echo ""
    echo "Branch naming: feat/ fix/ refactor/ chore/ docs/ test/"
    echo ""
    exit 1
  fi
done
