#!/usr/bin/env bash
# release.sh — semver release workflow helper
# Works with the semantic-release alpha→release/X.Y branching model.
#
# Managed by releaser — do not edit in place. Local changes are detected on
# `releaser upgrade`, which will show a diff and ask before replacing this file.
# releaser-template-version: 1.1.0-pre.6
set -euo pipefail

# ── colours ──────────────────────────────────────────────────────────────────
# Use $'...' so \033 is an actual ESC byte — works in both echo -e and printf.
RED=$'\033[0;31m'; YELLOW=$'\033[1;33m'; GREEN=$'\033[0;32m'
CYAN=$'\033[0;36m'; BOLD=$'\033[1m'; DIM=$'\033[2m'; RESET=$'\033[0m'
SELF="$0"

# semantic-release config. Its "branches" array is regenerated from git refs by
# regenerate_branches() so that the highest release/X.Y line is the current
# release (channel "latest") and every older line is a maintenance branch.
RELEASERC=".releaserc.json"

# CI directory carrying the release workflow — ".forgejo" or ".github" — and the
# human name of that CI system. Both are substituted by releaser when this file
# is deployed, so the messages below name the forge the project actually uses.
CI_DIR=".github"
CI_NAME="GitHub Actions"

info()    { echo -e "  ${CYAN}ℹ${RESET}  $*"; }
success() { echo -e "  ${GREEN}✔${RESET}  $*"; }
warn()    { echo -e "  ${YELLOW}⚠${RESET}  $*"; }
die()     { echo -e "\n  ${RED}✖${RESET}  $*\n" >&2; exit 1; }
header()  { echo -e "\n${BOLD}$*${RESET}"; }
rule()    { echo -e "${DIM}────────────────────────────────────────────${RESET}"; }

# ── git helpers ───────────────────────────────────────────────────────────────
current_branch() { git rev-parse --abbrev-ref HEAD 2>/dev/null || die "Not inside a git repository."; }

current_version() {
  if [[ -f VERSION.txt ]]; then
    tr -d '[:space:]' < VERSION.txt
  else
    git describe --tags --abbrev=0 2>/dev/null | sed 's/^v//' || echo "unknown"
  fi
}

latest_tag() {
  git describe --tags --abbrev=0 2>/dev/null | sed 's/^v//' || echo "none"
}

require_clean_tree() {
  if ! git diff --quiet || ! git diff --cached --quiet; then
    die "Working tree is dirty. Commit or stash your changes first."
  fi
}

require_branch() {
  local branch
  branch=$(current_branch)
  [[ "$branch" == "$1" ]] || \
    die "This command must be run from the '${BOLD}$1${RESET}' branch (currently on '${BOLD}${branch}${RESET}')."
}

branch_exists() {
  git show-ref --verify --quiet "refs/heads/$1" || \
  git show-ref --verify --quiet "refs/remotes/origin/$1"
}

# Returns true (0) if at least one release/* branch exists locally or on origin.
any_release_branch_exists() {
  git branch --list 'release/*' --format='%(refname:short)' 2>/dev/null | grep -q . || \
  git branch -r --list 'origin/release/*' --format='%(refname:short)' 2>/dev/null | grep -q .
}

# Prints "<tag> <release-branch> <commit>" for the newest graduation that has
# not reached HEAD's ancestry, and returns 1 if there is none. Used to detect
# whether `finalize` is needed.
#
# Only a line's X.Y.0 counts. Main needs to see each line's graduation to move
# on to the next minor; a patch X.Y.N on that line changes nothing for main, so
# it must not ask for another finalize.
#
# The proof of a graduation is the "chore(release): X.Y.0 [skip ci]" commit on
# the release line, not the tag. A tag alone can mislead: `bootstrap` seeds
# vX.Y.0 on a commit main already contains and never pushes it, so the tag looks
# merged while the commit CI wrote back (the one that carries the version and
# the changelog) is not. The tag scan below stays as a fallback for lines that
# were graduated before they had such a commit.
latest_full_release_not_in_ancestry() {
  local xy sha r tag found=""
  local -a refs
  while IFS= read -r xy; do
    [[ -n "$xy" ]] || continue
    # Both refs: a local line with unpushed backports has diverged from origin,
    # and only origin carries the commit CI wrote back.
    refs=()
    while IFS= read -r r; do refs+=("$r"); done < <(_line_refs "release/${xy}")
    # -F, not a regex: no escaping, so no difference between bash 3.2 and 4.3+
    # in how a backslash inside ${var//…} survives double quotes.
    sha=$(git log -1 -F --grep="chore(release): ${xy}.0 [skip ci]" \
            --format=%H "${refs[@]}" 2>/dev/null || true)
    [[ -n "$sha" ]] || continue
    git merge-base --is-ancestor "$sha" HEAD 2>/dev/null && continue
    found="v${xy}.0 release/${xy} ${sha}"
  done < <(release_lines)
  if [[ -n "$found" ]]; then echo "$found"; return 0; fi

  while IFS= read -r tag; do
    # ^{} dereferences annotated tags to the underlying commit
    if ! git merge-base --is-ancestor "${tag}^{}" HEAD 2>/dev/null; then
      echo "${tag} - $(git rev-parse "${tag}^{}")"
      return 0
    fi
  done < <(git tag --sort=-version:refname | grep -E '^v[0-9]+\.[0-9]+\.0$')
  return 1
}

# _warn_if_tag_unusable <tag> <release-commit>: semantic-release finds the
# last release by tag, so a full release only counts once vX.Y.Z is on origin
# and on the commit CI wrote back. Warns, with the commands, when it is not.
_warn_if_tag_unusable() {
  local tag="$1" sha="$2" local_sha="" bad=0 rc=0
  local_sha=$(git rev-parse -q --verify "refs/tags/${tag}^{}" 2>/dev/null || true)
  git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1 || rc=$?
  # --exit-code makes "no such ref" exit 2. Anything else is origin failing, and
  # telling the user to recreate a tag CI may well have published would be wrong.
  if (( rc == 2 )); then
    warn "${tag} is not on origin, so semantic-release cannot see it."
    bad=1
  elif (( rc != 0 )); then
    warn "Could not check origin for ${tag} (git ls-remote exit ${rc})."
  fi
  if [[ -n "$local_sha" && "$local_sha" != "$sha" ]]; then
    warn "Local ${tag} points at ${local_sha:0:7}, not at the release commit ${sha:0:7}."
    bad=1
  fi
  (( bad )) || return 0
  echo -e "  If CI never published ${tag}, tag the release commit and push it:"
  [[ -n "$local_sha" && "$local_sha" != "$sha" ]] && echo -e "    ${BOLD}git tag -d ${tag}${RESET}"
  echo -e "    ${BOLD}git tag -a ${tag} ${sha:0:7} -m \"Release ${tag#v}\"${RESET}"
  echo -e "    ${BOLD}git push origin ${tag}${RESET}"
  echo
}

# _release_commit_on <ref> <version>: true if <ref> carries the
# "chore(release): <version> [skip ci]" commit that @semantic-release/git writes
# back once a release has fully completed. git's own --grep, not `git log | grep -q`:
# under pipefail, grep -q exiting on the first match can SIGPIPE git log on a long
# history and turn exactly the success case into a failure.
_release_commit_on() {
  [[ -n "$(git log -1 -F --grep="chore(release): $2 [skip ci]" --format=%H "$1" 2>/dev/null)" ]]
}

# Derives the next minor version string from a full version string.
# e.g. "1.2.0" → "1.3.0"
next_minor() { echo "$1" | awk -F. '{print $1"."$2+1".0"}'; }

# ask_yes <prompt>  — returns 0 if user answers y/Y, 1 otherwise.
# Use: ask_yes "Proceed?" || { info "Aborted."; return; }
ask_yes() { local r; read -rp "  $1 [y/N] " r; [[ "$r" =~ ^[Yy]$ ]]; }

# ask_no <prompt>  — returns 0 unless user answers n/N (default yes).
# Use: if ask_no "Push now?"; then …; fi
ask_no()  { local r; read -rp "  $1 [Y/n] " r; ! [[ "$r" =~ ^[Nn]$ ]]; }

# _copy_from_main  — checkout the CI directory, build_tools/ and .releaserc.json
# from main onto the current branch. Carrying .releaserc.json is what keeps a
# release branch's view of the branch topology (which line is "latest", which are
# maintenance) in step with main after a new line has been cut.
#
# Each path is checked out only if it exists on main: a project may have no
# package-lock.json, and the CI directory differs per forge. Blindly checking
# out a missing path makes git exit non-zero, which under `set -e` would abort
# the whole command mid-way and leave a half-synced branch behind.
_copy_from_main() {
  local p
  for p in "${CI_DIR}/" build_tools/ package.json package-lock.json "$RELEASERC"; do
    # A tree path needs its trailing slash stripped for `git cat-file -e`.
    git cat-file -e "main:${p%/}" 2>/dev/null || continue
    git checkout main -- "$p"
  done
}

# release_lines — prints the release/X.Y lines (just the "X.Y" part), one per
# line, sorted ascending by version and de-duplicated across local + origin refs.
release_lines() {
  {
    git branch    --list 'release/*'        --format='%(refname:short)' 2>/dev/null
    git branch -r --list 'origin/release/*' --format='%(refname:short)' 2>/dev/null \
      | sed 's|^origin/||'
  } | sed -n 's|^release/||p' | sort -V -u
}

# _write_releaserc <path> <X.Y> [<X.Y> …]  — rewrite the "branches" array of the
# semantic-release config at <path> from the supplied ascending list of release
# lines. The highest line becomes the current release branch (channel "latest");
# every lower line becomes a maintenance branch (range "X.Y.x", channel "X.Y.x").
# The "main" prerelease entry is always appended. All other config (plugins, …)
# is preserved — only the branches array is replaced.
_write_releaserc() {
  local path="$1"; shift
  python3 - "$path" "$@" <<'PY'
import json, re, sys

path, *lines = sys.argv[1:]
with open(path) as f:
    text = f.read()

branches = []
for i, xy in enumerate(lines):
    if i == len(lines) - 1:                      # highest line → current release
        branches.append({"name": f"release/{xy}", "channel": "latest"})
    else:                                        # older line → maintenance
        branches.append({"name": f"release/{xy}",
                         "range": f"{xy}.x", "channel": f"{xy}.x"})
branches.append({"name": "main", "channel": "alpha", "prerelease": "pre"})

# Surgically replace only the "branches": [ … ] block so the rest of the config
# (plugin formatting, comments-as-keys, etc.) is preserved byte-for-byte. The
# block is the array of objects whose closing "]" sits at the 2-space top-level
# indent; branch objects never contain nested arrays, so this is unambiguous.
arr = json.dumps(branches, indent=2).split("\n")
body = "\n".join([arr[0]] + ["  " + ln for ln in arr[1:]])   # +2 to align under key
replacement = f'"branches": {body}'

new_text, n = re.subn(r'"branches":\s*\[.*?\n  \]', replacement, text,
                      count=1, flags=re.DOTALL)
if n != 1:
    sys.exit(f"could not locate the branches array in {path}")

with open(path, "w") as f:
    f.write(new_text)
PY
}

# regenerate_branches — rewrite .releaserc.json's branches array from git refs.
# Writes nothing and returns 1 if no release/* branch exists yet (pre-bootstrap).
regenerate_branches() {
  local lines
  lines=$(release_lines)
  [[ -n "$lines" ]] || return 1
  # shellcheck disable=SC2086 # intentional word-splitting; versions are token-safe
  _write_releaserc "$RELEASERC" $lines
}

# _releaserc_in_sync — returns 0 if .releaserc.json's branches array already
# matches what regenerate_branches would produce from the current git refs, 1 if
# it has drifted. Non-destructive: it renders the expected config into a temp
# copy and diffs. Returns 0 (nothing to check) before the first release branch.
_releaserc_in_sync() {
  local lines tmp rc
  lines=$(release_lines)
  [[ -n "$lines" ]] || return 0
  [[ -f "$RELEASERC" ]] || return 1
  tmp=$(mktemp)
  cp "$RELEASERC" "$tmp"
  # shellcheck disable=SC2086 # intentional word-splitting; versions are token-safe
  _write_releaserc "$tmp" $lines
  diff -q "$tmp" "$RELEASERC" >/dev/null 2>&1; rc=$?
  rm -f "$tmp"
  return $rc
}

# _token_var_for_ci — the env var holding the forge API token for $CI_DIR.
# Single source of truth so cmd_release and cmd_help can't drift.
_token_var_for_ci() {
  case "$CI_DIR" in
    .github)  echo "GITHUB_TOKEN" ;;
    .forgejo) echo "GITEA_TOKEN"  ;;
  esac
}

# ── cmd: status ───────────────────────────────────────────────────────────────
cmd_status() {
  header "Release Status"
  rule

  local branch version tag
  branch=$(current_branch)
  version=$(current_version)
  tag=$(latest_tag)

  printf "  %-18s %s\n" "Branch:"        "${BOLD}${branch}${RESET}"
  printf "  %-18s %s\n" "VERSION.txt:"   "${BOLD}${version}${RESET}"
  printf "  %-18s %s\n" "Latest tag:"    "${BOLD}v${tag}${RESET}"
  rule

  # ── bootstrap warning ────────────────────────────────────────────────────
  if ! any_release_branch_exists; then
    echo -e "  ${RED}${BOLD}⚠  Bootstrap required${RESET}"
    echo -e "  No ${CYAN}release/*${RESET} branch exists yet. semantic-release will fail on CI"
    echo -e "  because it requires at least one non-prerelease branch to be present."
    echo
    echo -e "  Run:  ${BOLD}${SELF} bootstrap${RESET}"
    rule
  fi

  case "$branch" in
    main)
      echo -e "  Mode: ${YELLOW}Pre-release (alpha)${RESET}"
      echo -e "  Commits here produce ${YELLOW}X.Y.0-pre.N${RESET} versions via CI."
      echo

      # ── finalize warning ───────────────────────────────────────────────────
      local unmerged_tag=""
      unmerged_tag=$(latest_full_release_not_in_ancestry || true)
      unmerged_tag=${unmerged_tag%% *}
      if [[ -n "$unmerged_tag" ]]; then
        local uv="${unmerged_tag#v}"
        local nv
        nv=$(next_minor "$uv")
        echo -e "  ${RED}${BOLD}⚠  Finalize required${RESET}"
        echo -e "  ${GREEN}${uv}${RESET} was graduated on a release branch but is not in main's"
        echo -e "  git ancestry. semantic-release can't see it and will keep producing"
        echo -e "  ${YELLOW}${uv%.*}.0-pre.N${RESET} forever instead of advancing to ${YELLOW}${nv}-pre.1${RESET}."
        echo
        echo -e "  Run:  ${BOLD}${SELF} finalize${RESET}"
        echo
      fi

      if [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)-pre\.([0-9]+)$ ]]; then
        local major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}"
        if [[ -z "$unmerged_tag" ]]; then
          info "Ready to ship ${major}.${minor}.0? Run:  ${BOLD}${SELF} cut${RESET}"
          info "That creates ${CYAN}release/${major}.${minor}${RESET} and CI graduates to ${GREEN}${major}.${minor}.0${RESET}."
          info "cut waits for CI, then offers ${BOLD}${SELF} finalize${RESET} so main advances to ${YELLOW}${major}.$((minor + 1)).0-pre.1${RESET}."
        fi
      elif [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
        info "No pre-release yet. Push a ${BOLD}feat${RESET} or ${BOLD}fix${RESET} commit to trigger one, or force it with ${BOLD}${SELF} graduate${RESET}."
      fi
      ;;
    release/*)
      local xy="${branch#release/}"
      echo -e "  Mode: ${GREEN}Stable patch line ${BOLD}${xy}${RESET}"
      echo -e "  Commits here produce ${GREEN}${xy}.N${RESET} patch releases via CI."
      echo
      # Warn if the workflow file is missing — CI will silently do nothing.
      local root
      root=$(git rev-parse --show-toplevel)
      if [[ ! -f "${root}/${CI_DIR}/workflows/release.yml" ]]; then
        echo -e "  ${RED}${BOLD}⚠  Workflow file missing on this branch!${RESET}"
        echo -e "  ${CI_NAME} cannot trigger CI here — the ${CYAN}${CI_DIR}/workflows/${RESET} directory"
        echo -e "  does not exist. This happens when a branch was created from an old"
        echo -e "  tag that predated the workflow being added to the repo."
        echo
        echo -e "  Fix:  ${BOLD}${SELF} sync${RESET}"
        echo
      fi
      info "Use ${BOLD}${SELF} commit${RESET} to add a conventional fix commit."
      info "Or fix it on main and copy it here with ${BOLD}${SELF} backport${RESET}."
      ;;
    *)
      warn "Not on 'main' or a 'release/*' branch — semantic-release won't run here."
      ;;
  esac

  # Show all release lines and their semantic-release role. With the maintenance
  # model only ONE release branch ("latest") plus the alpha prerelease branch
  # exist at any time — every older line is a maintenance branch, which does NOT
  # count toward semantic-release's 3-release-branch limit. So there is no
  # branch-count ceiling to police; instead we check the config hasn't drifted.
  local lines highest
  lines=$(release_lines)
  if [[ -n "$lines" ]]; then
    highest=$(echo "$lines" | tail -1)
    echo
    info "Release lines (from git refs):"
    while IFS= read -r xy; do
      if [[ "$xy" == "$highest" ]]; then
        echo -e "    ${CYAN}release/${xy}${RESET}  ${GREEN}latest${RESET}  ${DIM}(current release)${RESET}"
      else
        echo -e "    ${CYAN}release/${xy}${RESET}  maintenance  ${DIM}(${xy}.x channel)${RESET}"
      fi
    done <<< "$lines"

    if ! _releaserc_in_sync; then
      echo
      echo -e "  ${RED}${BOLD}⚠  ${RELEASERC} is out of sync with the release branches${RESET}"
      echo -e "  Its ${CYAN}branches${RESET} array no longer matches the actual ${CYAN}release/*${RESET} refs."
      echo -e "  semantic-release may pick the wrong channel for a line until this is fixed."
      echo
      echo -e "  Fix on this branch:  ${BOLD}${SELF} branches${RESET}"
      echo
    fi
  fi
  echo
}

# ── cmd: commit ───────────────────────────────────────────────────────────────
# Conventional commit types, shown in the menu.
# Format: "key|label|version-impact"
COMMIT_MENU=(
  "feat|feat: new feature|bumps MINOR on the next release"
  "fix|fix: bug fix|bumps PATCH on the next release"
  "docs|docs: documentation|bumps PATCH (README scope) or none"
  "refactor|refactor: code restructure, no behaviour change|bumps PATCH"
  "style|style: formatting / whitespace|bumps PATCH"
  "test|test: add or fix tests|no release"
  "perf|perf: performance improvement|bumps PATCH"
  "chore|chore: build / tooling / deps|no release"
  "ci|ci: CI configuration|no release"
  "revert|revert: revert a previous commit|depends on reverted commit"
)

cmd_commit() {
  header "New Conventional Commit"
  rule

  # Stage everything if nothing is staged yet
  if git diff --cached --quiet; then
    warn "Nothing is staged."
    local unstaged
    unstaged=$(git diff --name-only)
    if [[ -n "$unstaged" ]]; then
      echo -e "  Unstaged files:\n"
      while IFS= read -r f; do echo -e "    ${DIM}${f}${RESET}"; done <<< "$unstaged"
      echo
    fi
    read -rp "  Stage all changes now (git add -A)? [y/N] " stage_all
    [[ "$stage_all" =~ ^[Yy]$ ]] || die "Stage your changes first, then re-run."
    git add -A
    success "Staged all changes."
  fi

  rule
  echo -e "  Commit type:\n"
  local i=1
  for entry in "${COMMIT_MENU[@]}"; do
    IFS='|' read -r key label impact <<< "$entry"
    printf "  %2d)  %-40s ${DIM}%s${RESET}\n" "$i" "$label" "($impact)"
    ((i++))
  done
  echo

  local type_idx
  while true; do
    read -rp "  Choose type [1-${#COMMIT_MENU[@]}]: " type_idx
    if [[ "$type_idx" =~ ^[0-9]+$ ]] && (( type_idx >= 1 && type_idx <= ${#COMMIT_MENU[@]} )); then
      break
    fi
    warn "Enter a number between 1 and ${#COMMIT_MENU[@]}."
  done
  local type
  IFS='|' read -r type _ _ <<< "${COMMIT_MENU[$((type_idx - 1))]}"

  echo
  read -rp "  Scope (optional — e.g. auth, api, parser — Enter to skip): " scope

  local subject_prefix
  if [[ -n "$scope" ]]; then
    subject_prefix="${type}(${scope})"
  else
    subject_prefix="${type}"
  fi

  echo
  read -rp "  Short description: " description
  [[ -n "$description" ]] || die "Description cannot be empty."

  echo
  read -rp "  Extended body (optional — Enter to skip): " body

  echo
  local breaking_footer=""
  read -rp "  Breaking change? [y/N] " is_breaking
  if [[ "$is_breaking" =~ ^[Yy]$ ]]; then
    read -rp "  Describe the breaking change: " breaking_desc
    [[ -n "$breaking_desc" ]] || die "Breaking change description cannot be empty."
    breaking_footer="BREAKING CHANGE: ${breaking_desc}"
    # Also mark with ! in subject per Conventional Commits spec
    subject_prefix="${subject_prefix}!"
  fi

  # ── preview ──────────────────────────────────────────────────────────────
  local subject="${subject_prefix}: ${description}"
  rule
  echo -e "  ${BOLD}Preview:${RESET}\n"
  echo -e "  ${BOLD}${subject}${RESET}"
  [[ -n "$body" ]]            && echo -e "\n  ${body}"
  [[ -n "$breaking_footer" ]] && echo -e "\n  ${RED}${breaking_footer}${RESET}"
  echo
  rule

  read -rp "  Commit? [Y/n] " confirm
  [[ "$confirm" =~ ^[Nn]$ ]] && { info "Aborted — nothing committed."; return; }

  # Build full message
  local msg="$subject"
  [[ -n "$body" ]]            && msg+=$'\n\n'"$body"
  [[ -n "$breaking_footer" ]] && msg+=$'\n\n'"$breaking_footer"

  git commit -m "$msg"
  echo
  success "Committed:  ${BOLD}${subject}${RESET}"

  local branch
  branch=$(current_branch)
  case "$branch" in
    main)       info "Push to origin to trigger a pre-release build." ;;
    release/*)  info "Push to origin to trigger a patch release build." ;;
  esac
  echo
}

# ── cmd: bootstrap ───────────────────────────────────────────────────────────
# One-time setup: semantic-release requires at least one non-prerelease branch
# to exist on the remote. When switching an existing project to the
# main→pre-release model, the last full release that lived on main must be
# retroactively represented as a release/X.Y branch.
cmd_bootstrap() {
  header "Bootstrap Initial Release Branch"
  rule

  require_branch main
  require_clean_tree

  if any_release_branch_exists; then
    warn "Release branches already exist — bootstrap is a one-time setup."
    info "Use ${BOLD}${SELF} cut${RESET} to create future release branches."
    echo
    return
  fi

  local version
  version=$(current_version)

  # The last full (non-prerelease) tag, which is what a release branch has to be
  # cut from. Tags are the authority here, not VERSION.txt: a version can sit in
  # VERSION.txt without ever having been tagged, and `git rev-list` below needs
  # a tag that actually exists.
  local latest_full_tagged
  latest_full_tagged=$(git tag --sort=-version:refname \
                      | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' \
                      | head -1 \
                      | sed 's/^v//' || true)

  local full_version="$latest_full_tagged"
  # VERSION.txt wins when it names a full version that IS tagged — it is the
  # more precise statement of "which line are we on".
  if [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
     && git rev-parse --verify -q "refs/tags/v${version}" >/dev/null; then
    full_version="$version"
  fi

  # ── virgin repo: no full release has ever been tagged ────────────────────
  # semantic-release cannot invent the starting point itself — it needs a full
  # (non-prerelease) tag reachable from a release branch to anchor the version
  # series. In a project that has never released, seed that anchor here rather
  # than making the user hand-craft the tag.
  local seeded=0
  if [[ -z "$full_version" ]]; then
    git rev-parse --verify -q HEAD >/dev/null || \
      die "This repository has no commits yet. Make an initial commit and retry."

    echo
    echo -e "  ${YELLOW}${BOLD}No release has ever been made in this repository.${RESET}"
    echo -e "  semantic-release anchors the version series on a full (non-prerelease)"
    echo -e "  tag reachable from a release branch. Let's create that starting point."
    echo

    # If VERSION.txt already names a full version, honour it as the default —
    # the project has said which version it considers itself to be, it just
    # never tagged it.
    local seed_default="0.1.0" seed_version
    if [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then seed_default="$version"; fi
    read -rp "  Starting version [${seed_default}]: " seed_version
    seed_version="${seed_version:-$seed_default}"
    seed_version="${seed_version#v}"
    [[ "$seed_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || \
      die "'${seed_version}' is not a full X.Y.Z version."
    # `A && die` would abort the script under `set -e` on the common path where
    # the tag does not exist, so the check has to be a real conditional.
    if git rev-parse --verify -q "refs/tags/v${seed_version}" >/dev/null; then
      die "Tag 'v${seed_version}' already exists."
    fi

    echo
    echo -e "  This writes ${BOLD}VERSION.txt${RESET}, commits it on ${BOLD}main${RESET} and tags it"
    echo -e "  ${BOLD}v${seed_version}${RESET}. The first CI pre-release will then be"
    echo -e "  ${YELLOW}$(next_minor "$seed_version")-pre.1${RESET}."
    rule
    ask_yes "Create the starting release v${seed_version}?" || { info "Aborted."; return; }

    echo "$seed_version" > VERSION.txt
    git add VERSION.txt
    # VERSION.txt may already have said this, in which case there is nothing to
    # commit and the tag simply goes on HEAD. `git commit` with an empty index
    # fails, which would abort the whole bootstrap under `set -e`.
    if git diff --cached --quiet; then
      info "VERSION.txt already reads ${seed_version} — tagging HEAD."
    else
      git commit -m "${MSG_RELEASE}seed version ${seed_version} [skip ci]" >/dev/null
    fi
    git tag -a "v${seed_version}" -m "Release ${seed_version}"
    success "Created starting release ${GREEN}v${seed_version}${RESET} on main."
    full_version="$seed_version"
    seeded=1
    echo
  fi

  [[ "$full_version" =~ ^([0-9]+)\.([0-9]+)\.[0-9]+$ ]] || \
    die "Could not parse full version from '${full_version}'."

  local major="${BASH_REMATCH[1]}"
  local minor="${BASH_REMATCH[2]}"
  local release_branch="release/${major}.${minor}"

  echo
  echo -e "  semantic-release needs at least one ${GREEN}release branch${RESET} (non-prerelease)"
  if [[ $seeded -eq 1 ]]; then
    echo -e "  to exist on the remote. This creates it from the starting tag that was"
    echo -e "  just made above.\n"
    printf "  %-30s %s\n" "Starting release:"    "${GREEN}${full_version}${RESET}"
  else
    echo -e "  to exist on the remote. This creates it from the last tag that was"
    echo -e "  shipped directly on main, before the new branching model was adopted.\n"
    printf "  %-30s %s\n" "Last full release:"    "${GREEN}${full_version}${RESET}"
  fi
  printf "  %-30s %s\n" "Source tag:"           "${BOLD}v${full_version}${RESET}"
  printf "  %-30s %s\n" "Branch to create:"     "${CYAN}${release_branch}${RESET}"
  printf "  %-30s %s\n" "Effect on main CI:"    "pre-releases will now work"
  printf "  %-30s %s\n" "Effect on new branch:" "patch releases (${major}.${minor}.N) via CI"
  rule

  ask_yes "Proceed?" || { info "Aborted."; return; }

  # Branch from the exact tag commit for precision.
  local tag_commit
  tag_commit=$(git rev-list -n1 "v${full_version}" 2>/dev/null) || \
    die "Tag 'v${full_version}' not found locally. Run 'git fetch --tags' first."

  git checkout -b "$release_branch" "$tag_commit"
  success "Created ${BOLD}${release_branch}${RESET} at tag v${full_version}"
  echo

  # The tag predates the workflow files — copy them from main now.
  # Without this, ${CI_NAME} has no ${CI_DIR}/workflows/ to load and will
  # never trigger CI on this branch, regardless of the push filter.
  info "Copying workflow files from main (tag predates them)…"
  _copy_from_main
  # This is the only (so far) release line, so it becomes the "latest" release
  # branch. Fold the regenerated config into the workflow commit.
  regenerate_branches && git add "$RELEASERC"
  # A tag cut after the workflow files were added has nothing to stage, and
  # `git commit` on an empty index would abort the whole bootstrap.
  if git diff --cached --quiet; then
    info "Workflow files already present at the tag. Nothing to add."
  else
    git commit -m "${MSG_ADD_WORKFLOWS} [skip ci]" >/dev/null
    success "Workflow files added. ${CI_NAME} will run CI on this branch."
  fi
  echo

  # The seed tag has to reach origin with the branch. Left local, CI has no
  # full release to anchor on and computes its own X.Y.0 on the new branch,
  # which then collides with the seed and leaves main unable to see either.
  local -a push_refs=("$release_branch")
  (( seeded )) && push_refs+=("refs/tags/v${full_version}")

  if ask_no "Push ${push_refs[*]#refs/tags/} to origin now?"; then
    git push -u origin "${push_refs[@]}"
    success "Pushed — semantic-release on main will now pass validation"
    echo
    info "Returning to main…"
    git checkout main
    echo
    # Make main aware of the freshly-created release line too.
    local main_ahead=0 ahead
    # An origin/main that cannot be compared counts as "ahead": ask rather than skip.
    ahead=$(git rev-list --count origin/main..main 2>/dev/null) || ahead=1
    (( ahead > 0 )) && main_ahead=1
    if _regen_and_commit "${MSG_REGISTER}${release_branch}${MSG_REGISTER_TAIL} [skip ci]"; then
      success "Updated ${RELEASERC} on main."
      main_ahead=1
    fi
    # semantic-release on main only sees the tag if its commit is on origin/main,
    # and the seed commit exists only locally until main is pushed.
    if (( main_ahead )); then
      (( seeded )) && info "v${full_version} is only visible to main's CI once main is pushed."
      if ask_no "Push main now?"; then git push origin main; fi
      echo
    fi
    success "Done. Push a ${BOLD}feat${RESET} or ${BOLD}fix${RESET} commit on main to trigger the first pre-release."
  else
    warn "Branch created locally but NOT pushed yet."
    echo -e "  Push when ready:  ${BOLD}git push -u origin ${push_refs[*]#refs/tags/}${RESET}"
    (( seeded )) && echo -e "  The tag matters: without it CI graduates its own ${full_version} and collides."
    echo -e "  Then:             ${BOLD}git checkout main && git push${RESET}"
  fi
  echo
}

# ── cmd: finalize ────────────────────────────────────────────────────────────
# After a release/X.Y branch graduates (CI produces vX.Y.0), that tag lives on
# the release branch — NOT in main's git ancestry.  semantic-release on main
# only sees tags reachable from main, so it keeps producing X.Y.0-pre.N forever.
#
# Fix: merge the graduation commit into main using the "ours" strategy.  This
# creates a merge commit whose second parent is that commit, making
# vX.Y.0 reachable from main.  The working tree is untouched (ours = keep
# main's content).  After pushing, the next feat/fix commit will produce the
# correct X.Y+1.0-pre.1.
cmd_finalize() {
  header "Finalize — Advance main past the graduated release"
  rule

  require_branch main
  require_clean_tree

  # CI writes the graduation commit onto origin's release branch, so look there
  # before deciding. Branches only: with --tags, one stale local tag (a seed that
  # was never pushed, say) would make the whole fetch fail with "would clobber".
  info "Fetching release branches from origin…"
  git fetch --quiet origin '+refs/heads/release/*:refs/remotes/origin/release/*' 2>/dev/null \
    || warn "Could not reach origin. Deciding from the refs already fetched."

  local full_tag release_branch release_sha pending
  pending=$(latest_full_release_not_in_ancestry) || {
    success "All full releases are already in main's ancestry — nothing to finalize."
    info "Push a feat/fix commit to trigger the next pre-release cycle."
    echo
    return
  }
  read -r full_tag release_branch release_sha <<< "$pending"

  local full_version="${full_tag#v}"
  local next_v
  next_v=$(next_minor "$full_version")

  # A tag-only match carries no branch: find the release/* branch containing it.
  # %(refname:short) already strips "refs/remotes/", so remote tracking refs
  # look like "origin/release/1.4" — strip the leading "origin/" to normalise.
  if [[ "$release_branch" == "-" ]]; then
    release_branch=$(git branch -a --contains "${full_tag}^{}" --format='%(refname:short)' \
                    | sed 's|^origin/||' \
                    | grep '^release/' \
                    | sort -u | head -1)
  fi

  [[ -n "$release_branch" ]] || \
    die "Cannot find a release/* branch containing ${full_tag}.\nRun 'git fetch --tags' and retry."

  echo -e "  ${GREEN}${full_version}${RESET} was graduated on ${CYAN}${release_branch}${RESET}"
  echo -e "  but its release commit is not in ${BOLD}main${RESET}'s ancestry.\n"
  echo -e "  A merge commit will be added to main using the ${DIM}ours${RESET} strategy:"
  echo -e "   • Main's working tree and history are ${BOLD}unchanged${RESET}"
  echo -e "   • The merge parent records ${CYAN}${release_branch}${RESET}, making ${full_tag} reachable"
  echo -e "   • semantic-release will then see ${GREEN}${full_version}${RESET} and advance to ${YELLOW}${next_v}-pre.1${RESET}\n"
  printf "  %-28s %s\n" "Release to acknowledge:" "${GREEN}${full_version}${RESET}"
  printf "  %-28s %s\n" "From branch:"            "${CYAN}${release_branch}${RESET}"
  printf "  %-28s %s\n" "Next pre-release cycle:" "${YELLOW}${next_v}-pre.1${RESET}"
  rule

  ask_yes "Proceed?" || { info "Aborted."; return; }

  # Merge the graduation commit itself, not the branch by name: the local branch
  # may be stale or carry unpushed backports, and either way it need not contain
  # the commit that makes ${full_tag} reachable.
  git merge --no-ff -s ours \
    -m "${MSG_ADVANCE}${full_version} [skip ci]" \
    "${release_sha}"
  echo
  success "Merge complete — ${full_tag} is now reachable from main"
  info "main's files are unchanged; only the merge parentage was recorded."
  echo
  _warn_if_tag_unusable "$full_tag" "$release_sha"

  if ask_no "Push now?"; then
    git push
    success "Pushed — the next feat/fix commit on main will produce ${YELLOW}${next_v}-pre.1${RESET}"
  else
    warn "Not pushed yet."
    echo -e "  Push when ready:  ${BOLD}git push${RESET}"
  fi
  echo
}

# ── cmd: sync ────────────────────────────────────────────────────────────────
# Copies the CI directory, build_tools/ and .releaserc.json from main onto the
# current release branch. Needed when bootstrap created the branch from an old
# tag that predated the workflow files (without them CI never triggers), and to
# pick up an updated branch topology after the line was demoted to maintenance.
cmd_sync() {
  header "Sync Workflow Files + Config to Release Branch"
  rule

  local branch
  branch=$(current_branch)
  [[ "$branch" == release/* ]] || \
    die "Must be on a release/* branch. Currently on '${BOLD}${branch}${RESET}'.\n  Run ${BOLD}${SELF} switch${RESET} first."

  require_clean_tree

  local root
  root=$(git rev-parse --show-toplevel)
  local workflow_file="${root}/${CI_DIR}/workflows/release.yml"

  echo -e "  Copies ${CYAN}${CI_DIR}/${RESET}, ${CYAN}build_tools/${RESET} and ${CYAN}${RELEASERC}${RESET} from ${BOLD}main${RESET} onto ${BOLD}${branch}${RESET}."
  echo -e "  After pushing, ${CI_NAME} will run the release workflow. If the branch"
  echo -e "  has unreleased pre-release commits, graduation happens automatically.\n"

  if [[ -f "$workflow_file" ]]; then
    warn "Workflow file already exists on this branch."
    ask_yes "Overwrite with the version from main anyway?" || { info "Aborted."; return; }
  else
    rule
    ask_yes "Proceed?" || { info "Aborted."; return; }
  fi

  _copy_from_main

  if git diff --cached --quiet; then
    success "Workflow files + config already identical to main — nothing to commit."
    echo
    return
  fi

  git commit -m "${MSG_SYNC} and ${RELEASERC} from main"
  echo
  success "Committed workflow files + ${RELEASERC} onto ${BOLD}${branch}${RESET}"
  echo

  if ask_no "Push now?"; then
    git push
    success "Pushed — ${CI_NAME} will now run CI on ${BOLD}${branch}${RESET} pushes"
  else
    warn "Not pushed yet."
    echo -e "  Push when ready:  ${BOLD}git push${RESET}"
  fi
  echo
}

# ── cmd: cut ─────────────────────────────────────────────────────────────────
# _cut_push_and_return <release_branch> <full_version> <next_minor_ver>
# Shared push + return-to-main logic used by both the normal and recovery paths.
_cut_push_and_return() {
  local release_branch="$1" full_version="$2" next_minor_ver="$3"

  if ask_no "Push '${release_branch}' to origin now?"; then
    git push -u origin "$release_branch"
    success "Pushed ${release_branch} → CI will graduate to ${GREEN}${full_version}${RESET}"
    echo
    info "Returning to main…"
    git checkout main
    echo

    # Bring main's branch topology in step with the new line. The new branch was
    # cut before this point, so it already carries the regenerated config; main
    # is updated here. [skip ci] keeps this config-only commit from triggering a
    # spurious pre-release on main.
    if _regen_and_commit "${MSG_REGEN} for ${release_branch} [skip ci]"; then
      success "Updated ${RELEASERC} on main to reflect the new branch topology."
      if ask_no "Push main now?"; then git push; fi
      echo
    fi

    # The previously-latest line (now maintenance) keeps its own copy of
    # .releaserc.json; sync it so its next patch publishes on the X.Y.x channel
    # rather than clobbering "latest".
    local demoted
    demoted=$(release_lines | tail -2 | head -1)
    if [[ -n "$demoted" && "$demoted" != "${release_branch#release/}" ]]; then
      info "Demoted line ${CYAN}release/${demoted}${RESET} needs its config synced:"
      echo -e "  ${BOLD}${SELF} switch${RESET}  →  ${BOLD}${SELF} sync${RESET}   (copies the updated ${RELEASERC} from main)"
      echo
    fi

    if _wait_for_graduation "$release_branch" "$full_version"; then
      echo
      if ask_no "Run finalize now?"; then cmd_finalize; return; fi
    fi
    echo
    warn "Once CI finishes graduating ${full_version}, run:"
    echo -e "  ${BOLD}${SELF} finalize${RESET}"
    echo -e "  This merges the graduation back into main so semantic-release"
    echo -e "  can see ${GREEN}v${full_version}${RESET} and advance to ${YELLOW}${next_minor_ver}-pre.1${RESET}."
  else
    warn "Branch created locally but NOT pushed yet."
    echo -e "  Push when ready:  ${BOLD}git push -u origin ${release_branch}${RESET}"
    echo -e "  Then run:         ${BOLD}${SELF} finalize${RESET}"
  fi
  echo
}

# _wait_for_graduation <release_branch> <full_version>: block until CI has
# published <full_version> on origin, so `cut` can go straight on to `finalize`
# instead of leaving the user to remember it. Polls `git ls-remote` every
# RELEASE_POLL_SECONDS (default 15); on a terminal any key checks at once and q
# gives up. Without a terminal nobody can press q, so it gives up by itself after
# RELEASE_WAIT_SECONDS (default 1800). Nothing is written while waiting, so
# Ctrl-C is always safe. Returns 0 once the release commit is fetched locally,
# 1 if waiting stopped or origin is in a state finalize cannot use.
#
# The signal is the branch head moving past what was just pushed, together with
# the tag. The tag alone is not enough: when cut retries a graduation that CI
# tagged and then abandoned, the tag is on origin before this run even starts.
# @semantic-release/git pushes the release commit before semantic-release
# pushes the tag, so a new tag on an unmoved head means CI failed in between.
#
# Optional <verb> (default "graduate") words the status line, and a fourth
# argument of 1 applies the RELEASE_WAIT_SECONDS cap on a terminal as well, for a
# caller that waits on a version it only predicted.
_wait_for_graduation() {
  local branch="$1" v="$2" verb="${3:-graduate}" cap_tty="${4:-0}"
  local tag="v${v}" interval="${RELEASE_POLL_SECONDS:-15}" max="${RELEASE_WAIT_SECONDS:-1800}"
  [[ "$interval" =~ ^[1-9][0-9]*$ ]] || interval=15
  [[ "$max" =~ ^[1-9][0-9]*$ ]] || max=1800

  local tty=0
  [[ -t 0 ]] && tty=1

  local pushed tag_was_there=0
  pushed=$(git rev-parse "refs/heads/${branch}")
  git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1 && tag_was_there=1

  echo -e "  ⏳ Waiting for CI to ${verb} ${GREEN}${v}${RESET} on ${CYAN}${branch}${RESET}…"
  if (( tty )); then
    echo -e "     ${DIM}checking origin every ${interval}s · any key = check now · q = stop waiting${RESET}"
  fi

  local start=$SECONDS rc t0 key elapsed warned=0 out head has_tag
  while true; do
    rc=0
    out=$(git ls-remote --exit-code origin "refs/heads/${branch}" "refs/tags/${tag}" 2>/dev/null) || rc=$?
    if (( rc == 0 )); then
      head=$(awk -v r="refs/heads/${branch}" '$2 == r { print $1 }' <<< "$out")
      has_tag=0
      awk -v r="refs/tags/${tag}" '$2 == r { f = 1 } END { exit !f }' <<< "$out" && has_tag=1
      if (( has_tag )) && [[ -n "$head" && "$head" != "$pushed" ]]; then
        break
      fi
      if (( has_tag && ! tag_was_there )) && [[ "$head" == "$pushed" ]]; then
        (( tty )) && echo
        warn "${tag} appeared on origin but ${branch} did not move."
        echo -e "  CI tagged the release and then failed before writing it back."
        echo -e "  Check the ${CI_NAME} run on ${branch}."
        return 1
      fi
    # --exit-code makes "no such ref" exit 2; anything else is origin itself failing.
    elif (( rc != 2 && ! warned )); then
      (( tty )) && echo
      warn "Cannot reach origin (git ls-remote exit ${rc}). Still trying."
      warned=1
    fi

    if (( (! tty || cap_tty) && SECONDS - start >= max )); then
      (( tty )) && echo
      info "Stopped waiting after ${max}s. CI keeps running on ${branch}."
      return 1
    fi

    if (( tty )); then
      elapsed=$(( SECONDS - start ))
      printf '\r     %s[%02d:%02d] not yet%s' "$DIM" $(( elapsed / 60 )) $(( elapsed % 60 )) "$RESET"
      t0=$SECONDS; key=""; rc=0
      read -rsn1 -t "$interval" key || rc=$?
      if (( rc == 0 )) && [[ "$key" == [qQ] ]]; then
        echo
        info "Stopped waiting. CI keeps running on ${branch}."
        return 1
      fi
      # A timeout is 1 on bash 3.2 and >128 on bash 4+, so the status alone cannot
      # tell it from EOF. EOF returns at once with no key: from then on, sleep,
      # or this loop would hammer origin with ls-remote.
      if (( rc != 0 )) && [[ -z "$key" ]] && (( SECONDS - t0 < interval - 1 )); then
        tty=0
        echo
      fi
    else
      sleep "$interval"
    fi
  done
  (( tty )) && echo

  # Bring both in explicitly, so finalize and the status line see them at once.
  local err
  if ! err=$(git fetch --quiet origin \
               "+refs/heads/${branch}:refs/remotes/origin/${branch}" \
               "refs/tags/${tag}:refs/tags/${tag}" 2>&1); then
    warn "${tag} exists on origin but could not be fetched:"
    echo -e "  ${DIM}${err}${RESET}"
    return 1
  fi

  if ! _release_commit_on "refs/remotes/origin/${branch}" "$v"; then
    warn "${branch} moved on origin but has no ${DIM}chore(release): ${v}${RESET} commit."
    echo -e "  Something other than the graduation was pushed. Check the ${CI_NAME} run on ${branch}."
    return 1
  fi

  success "${v} is released on origin (tag ${tag})"
  return 0
}

# _regen_and_commit <commit-message> — regenerate the branches array on the
# current branch and, if .releaserc.json actually changed, stage + commit it.
# Returns 0 if a commit was made, 1 if there was nothing to do.
_regen_and_commit() {
  local msg="$1"
  regenerate_branches || return 1
  git diff --quiet -- "$RELEASERC" && return 1
  git add "$RELEASERC"
  git commit -m "$msg" >/dev/null
  return 0
}

# _add_trigger_commit <release_branch> <full_version> <return_branch>
# Check out the line, bring it level with origin, and put the empty trigger commit
# on it. A stale local branch would get the commit and its push would then be
# rejected as non-fast-forward with the commit left behind, so a diverged line
# stops here and <return_branch> is checked out again before dying.
_add_trigger_commit() {
  local branch="$1" v="$2" back="$3"
  git checkout --quiet "$branch"
  if git show-ref --verify --quiet "refs/remotes/origin/${branch}" \
     && ! git merge --ff-only --quiet "origin/${branch}" 2>/dev/null; then
    git checkout --quiet "$back"
    die "${branch} has diverged from origin/${branch}. Reconcile it, then try again."
  fi
  git commit --allow-empty -m "${MSG_TRIGGER}${v}"
}

# ── cmd: graduate ────────────────────────────────────────────────────────────
# Force a release with an empty commit. Two cases, told apart by the branch:
#
#   release/X.Y   graduate that line to X.Y.0. cut does the same when it finds a
#                 half-finished graduation, but only from main; this works from
#                 the release branch itself, or for a line named on the command
#                 line. It never creates the branch (that is cut's job) and
#                 refuses a line that is already graduated: an empty commit is
#                 not releasable, so CI would publish nothing and the next patch
#                 needs a real fix commit.
#   main          publish the next pre-release, see _graduate_main.
cmd_graduate() {
  header "Force Graduation"
  rule

  require_clean_tree

  local start line
  start=$(current_branch)
  if [[ "${1:-}" == "main" || ( -z "${1:-}" && "$start" == "main" ) ]]; then
    _graduate_main
    return
  fi
  line="${1:-$start}"
  line="${line#release/}"
  [[ "$line" =~ ^([0-9]+)\.([0-9]+)$ ]] || \
    die "Not on a release branch (on '${start}'). Run it from release/X.Y or main, or name the line: ${BOLD}${SELF} graduate X.Y${RESET}"

  local release_branch="release/${line}"
  local full_version="${line}.0"
  local next_minor_ver
  next_minor_ver=$(next_minor "$full_version")

  git fetch --quiet origin "+refs/heads/${release_branch}:refs/remotes/origin/${release_branch}" 2>/dev/null \
    || warn "Could not fetch ${release_branch} from origin. Deciding from the refs already fetched."
  branch_exists "$release_branch" || \
    die "Branch ${release_branch} does not exist. Create it from main with ${BOLD}${SELF} cut${RESET}."

  local r
  while IFS= read -r r; do
    if _release_commit_on "$r" "$full_version"; then
      success "${full_version} is already fully released."
      info "An empty commit publishes nothing. Patch releases need a ${BOLD}fix${RESET} commit: ${BOLD}${SELF} switch${RESET}"
      echo
      return
    fi
  done < <(_line_refs "$release_branch")

  printf "  %-28s %s\n" "Line:"                 "${CYAN}${release_branch}${RESET}"
  printf "  %-28s %s\n" "CI will graduate to:"  "${GREEN}${full_version}${RESET}"
  printf "  %-28s %s\n" "Commit to add:"        "${DIM}${MSG_TRIGGER}${full_version} (empty)${RESET}"
  rule
  ask_yes "Add the trigger commit and push ${release_branch}?" || { info "Aborted, nothing changed."; return; }
  echo

  _add_trigger_commit "$release_branch" "$full_version" "$start"
  if git push -u origin "$release_branch"; then
    success "Pushed ${release_branch}. CI will graduate to ${GREEN}${full_version}${RESET}"
    echo
    [[ "$start" == "$release_branch" ]] || git checkout --quiet "$start"
    if _wait_for_graduation "$release_branch" "$full_version"; then
      if latest_full_release_not_in_ancestry >/dev/null; then
        echo
        info "Now run ${BOLD}${SELF} finalize${RESET} on main so it advances to ${YELLOW}${next_minor_ver}-pre.1${RESET}."
      fi
    fi
  else
    [[ "$start" == "$release_branch" ]] || git checkout --quiet "$start"
    warn "The push failed. The trigger commit is on ${release_branch}; push it when ready:"
    echo -e "  ${BOLD}git push -u origin ${release_branch}${RESET}"
  fi
  echo
}

# _next_pre_release: print the version the next pre-release on main will carry,
# or nothing when it cannot be told (no stable tag in main's history yet).
#
# semantic-release numbers a pre-release from the last stable tag plus the highest
# bump since. The trigger commit is a minor, so: if the minor after the last
# stable tag is above the version main is on, the cycle moves to it and starts at
# pre.1 (right after finalize, or when a fix-only cycle X.Y.Z is bumped). If not,
# main is already on that minor or higher, and the counter simply goes up.
_next_pre_release() {
  local version base n last nm top
  version=$(current_version)
  [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+)(-pre\.([0-9]+))?$ ]] || return 0
  base="${BASH_REMATCH[1]}"
  n="${BASH_REMATCH[3]:-0}"
  last=$(git tag --merged HEAD --list 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null \
           | sed 's/^v//' | grep -Ev -- '-' | sort -V | tail -1 || true)
  [[ -n "$last" ]] || return 0
  nm=$(next_minor "$last")
  top=$(printf '%s\n%s\n' "$base" "$nm" | sort -V | tail -1)
  if [[ "$top" == "$nm" && "$base" != "$nm" ]]; then
    echo "${nm}-pre.1"
  else
    echo "${base}-pre.$((n + 1))"
  fi
}

# _graduate_main: publish the next pre-release by pushing an empty
# chore(pre-release) commit on main. Nothing else releases without a feat or fix,
# so after finalize, or after a run of docs and chore commits, main would sit
# still. The commit is invisible in the CHANGELOG, and the template's
# releaseRules turn it into a minor bump.
_graduate_main() {
  local start
  start=$(current_branch)
  [[ "$start" == "main" ]] || die "Run this from main (you are on '${start}')."

  git fetch --quiet origin "+refs/heads/main:refs/remotes/origin/main" 2>/dev/null \
    || warn "Could not fetch main from origin. Deciding from the refs already fetched."
  if git show-ref --verify --quiet refs/remotes/origin/main \
     && ! git merge-base --is-ancestor refs/remotes/origin/main HEAD; then
    die "main is behind or has diverged from origin/main. Pull it first, then try again."
  fi

  # Older deployments predate the rule, and a project may have changed it: either
  # way the commit would be pushed and publish nothing, or the wrong version.
  # python3, like the rest of this script; jq is not required of a project.
  local rule_rc=0
  RELEASERC_PATH="$RELEASERC" python3 - <<'PY' || rule_rc=$?
import json, os, sys
try:
    with open(os.environ["RELEASERC_PATH"]) as f:
        plugins = json.load(f).get("plugins", [])
except (OSError, ValueError):
    sys.exit(2)
rules = [r for p in plugins if isinstance(p, list) and "commit-analyzer" in str(p[0])
         for r in (p[1].get("releaseRules", []) if len(p) > 1 and isinstance(p[1], dict) else [])
         if r.get("type") == "chore" and r.get("scope") == "pre-release"]
if not rules:
    sys.exit(3)
sys.exit(0 if all(r.get("release") in ("minor", "major") for r in rules) else 4)
PY
  case "$rule_rc" in
    0) ;;
    2) die "Cannot read ${RELEASERC} as JSON." ;;
    3) die "${RELEASERC} has no release rule for chore(pre-release) commits, so the trigger would publish nothing.\n  Run releaser upgrade, commit the result, then try again." ;;
    *) die "The chore(pre-release) rule in ${RELEASERC} does not release a minor.\n  After finalize it would publish the wrong version or none. Set \"release\": \"minor\", or run releaser upgrade." ;;
  esac

  if latest_full_release_not_in_ancestry >/dev/null; then
    die "A graduated release is not in main's history yet, so main would keep producing pre-releases of the old version.\n  Run ${BOLD}${SELF} finalize${RESET} first."
  fi

  local version expected
  version=$(current_version)
  expected=$(_next_pre_release)
  [[ -n "$expected" ]] || \
    die "Cannot tell which version CI would publish: no stable vX.Y.Z tag is reachable from main.\n  Fetch the tags (${BOLD}git fetch --tags${RESET}) and try again. A shallow clone hides them."

  printf "  %-28s %s\n" "Current version:"   "${YELLOW}${version}${RESET}"
  printf "  %-28s %s\n" "Next pre-release:"  "${YELLOW}${expected}${RESET}"
  printf "  %-28s %s\n" "Commit to add:"     "${DIM}${MSG_PRE_TRIGGER}${expected} (empty)${RESET}"
  # A fix-only cycle X.Y.Z (Z > 0) is only a patch line; the trigger is a minor.
  if [[ "$version" =~ ^[0-9]+\.[0-9]+\.([0-9]+)-pre\. ]] \
     && (( BASH_REMATCH[1] > 0 )) && [[ "$expected" != "${version%-pre.*}-pre."* ]]; then
    warn "main is on a patch cycle (${version}); the trigger is a minor, so it moves to ${expected}."
  fi
  rule
  ask_yes "Add the trigger commit and push main?" || { info "Aborted, nothing changed."; return; }
  echo

  git commit --allow-empty -m "${MSG_PRE_TRIGGER}${expected}"
  # Explicit remote and ref: a bare `git push` follows the upstream and push.default,
  # which may be somewhere else, or nothing.
  if git push origin main; then
    success "Pushed main. CI will publish ${YELLOW}${expected}${RESET}"
    echo
    # The version is predicted, not read, so the wait is capped even on a terminal.
    if _wait_for_graduation main "$expected" publish 1; then
      info "CI wrote the release commit to origin/main. Run ${BOLD}git pull${RESET} to receive it."
    fi
  else
    warn "The push failed. The trigger commit is on main; push it when ready:"
    echo -e "  ${BOLD}git push origin main${RESET}"
  fi
  echo
}

cmd_cut() {
  header "Cut Release Branch"
  rule

  require_branch main
  require_clean_tree

  local version
  version=$(current_version)

  # Must be sitting on a pre-release version produced by CI
  if ! [[ "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)-pre\.([0-9]+)$ ]]; then
    die "VERSION.txt contains '${version}', which is not a pre-release (X.Y.Z-pre.N).\n" \
        "  Push at least one conventional commit to main and let CI run first."
  fi

  local major="${BASH_REMATCH[1]}"
  local minor="${BASH_REMATCH[2]}"
  local patch="${BASH_REMATCH[3]}"
  local release_branch="release/${major}.${minor}"
  local full_version="${major}.${minor}.${patch}"
  local next_minor_ver
  next_minor_ver=$(next_minor "$full_version")

  echo
  printf "  %-28s %s\n" "Current pre-release:"   "${YELLOW}${version}${RESET}"
  printf "  %-28s %s\n" "Branch to create:"      "${CYAN}${release_branch}${RESET}"
  printf "  %-28s %s\n" "CI will graduate to:"   "${GREEN}${full_version}${RESET}"
  printf "  %-28s %s\n" "main will then target:" "${YELLOW}${next_minor_ver}-pre.1${RESET}"
  rule

  # ── recovery path: branch exists but graduation may be incomplete ─────────
  if branch_exists "$release_branch"; then
    # A fully graduated release always has a "chore(release): X.Y.Z [skip ci]"
    # commit written back to the branch by @semantic-release/git.
    # A git tag alone is not sufficient: CI can create the tag and then fail
    # before writing the commit, leaving the branch in a broken half-state.
    # Either copy counts: after a graduation the commit is only on
    # origin/<branch>, while the local branch still stops at the trigger commit.
    git fetch --quiet origin "+refs/heads/${release_branch}:refs/remotes/origin/${release_branch}" 2>/dev/null \
      || warn "Could not fetch ${release_branch} from origin. Deciding from the refs already fetched."
    local graduated=0 r
    while IFS= read -r r; do
      _release_commit_on "$r" "$full_version" && graduated=1
    done < <(_line_refs "$release_branch")

    if (( graduated )); then
      success "${full_version} is already fully released."
      # main still reads X.Y.Z-pre.N until finalize has put the release in its
      # ancestry; that is the only reason to land here with a pre-release.
      if latest_full_release_not_in_ancestry >/dev/null; then
        warn "It is not in main's ancestry yet, so main keeps producing ${full_version%.*}.0-pre.N."
        if ask_no "Run finalize now?"; then cmd_finalize; return; fi
        info "Run ${BOLD}${SELF} finalize${RESET}, then push a feat/fix commit."
      fi
      info "For patch releases: ${BOLD}${SELF} switch${RESET}"
      echo
      return
    fi

    # Branch exists but release commit is absent → graduation didn't complete.
    warn "Branch '${release_branch}' exists but graduation to ${full_version} is incomplete."
    echo -e "  Common causes:"
    echo -e "   • HEAD commit had ${DIM}[skip ci]${RESET} when pushed — ${CI_NAME} skipped the workflow"
    echo -e "   • CI ran but failed before ${DIM}@semantic-release/git${RESET} could write back\n"
    read -rp "  Add a trigger commit and push to retry? [Y/n] " fix_it
    if ! [[ "$fix_it" =~ ^[Nn]$ ]]; then
      _add_trigger_commit "$release_branch" "$full_version" main
      _cut_push_and_return "$release_branch" "$full_version" "$next_minor_ver"
    else
      info "Aborted. Push a non-[skip ci] commit to ${release_branch} when ready."
    fi
    return
  fi

  # ── normal path ──────────────────────────────────────────────────────────

  # The previous "latest" line (if any) is demoted to a maintenance branch when
  # this new line takes over channel "latest". Surface that so it isn't a
  # surprise. Maintenance branches do NOT count toward semantic-release's
  # 3-release-branch limit, so there is no branch-count ceiling to police here.
  local prev_latest
  prev_latest=$(release_lines | tail -1)
  if [[ -n "$prev_latest" ]]; then
    echo -e "  ${CYAN}release/${prev_latest}${RESET} will be demoted to a ${BOLD}maintenance${RESET} branch"
    echo -e "  (range ${BOLD}${prev_latest}.x${RESET}, channel ${BOLD}${prev_latest}.x${RESET}); ${CYAN}${release_branch}${RESET} takes over ${GREEN}latest${RESET}."
    echo
  fi
  ask_yes "Proceed?" || { info "Aborted."; return; }

  git checkout -b "$release_branch"
  success "Created branch: ${BOLD}${release_branch}${RESET}"

  # Sync the branch topology on the new branch: it now matches release/X.Y, so
  # regenerate_branches makes it the "latest" release branch and demotes the
  # former latest line to maintenance. Staged here so it folds into the trigger
  # commit below (the commit is therefore no longer empty).
  regenerate_branches && git add "$RELEASERC"

  # The HEAD commit inherited from main is always a chore(release) commit with
  # [skip ci], which prevents CI from running the release workflow.
  # A trigger commit without [skip ci] (carrying the regenerated branches
  # config) fixes this.
  git commit --allow-empty \
    -m "${MSG_TRIGGER}${full_version}"
  echo

  _cut_push_and_return "$release_branch" "$full_version" "$next_minor_ver"
}

# ── cmd: switch ───────────────────────────────────────────────────────────────
cmd_switch() {
  header "Switch to Release Branch"
  rule

  # Collect local release/* branches
  local local_branches=()
  while IFS= read -r b; do
    [[ -n "$b" ]] && local_branches+=("$b")
  done < <(git branch --list 'release/*' --format='%(refname:short)' 2>/dev/null || true)

  # Collect remote-only release/* branches (not yet checked out locally)
  local remote_only=()
  while IFS= read -r b; do
    local short="${b#origin/}"
    [[ -n "$short" ]] || continue
    local found=0
    for lb in "${local_branches[@]+"${local_branches[@]}"}"; do
      [[ "$lb" == "$short" ]] && found=1 && break
    done
    [[ "$found" -eq 0 ]] && remote_only+=("$short")
  done < <(git branch -r --list 'origin/release/*' --format='%(refname:short)' 2>/dev/null || true)

  local all=("${local_branches[@]+"${local_branches[@]}"}" "${remote_only[@]+"${remote_only[@]}"}")

  [[ ${#all[@]} -gt 0 ]] || die "No release/* branches found (local or remote). Run '${SELF} cut' first."

  echo -e "  Available release branches:\n"
  local i=1
  for b in "${all[@]}"; do
    local tag=""
    for rb in "${remote_only[@]+"${remote_only[@]}"}"; do
      [[ "$rb" == "$b" ]] && tag=" ${DIM}(remote only)${RESET}" && break
    done
    printf "  %2d)  ${CYAN}%s${RESET}%b\n" "$i" "$b" "$tag"
    ((i++))
  done
  echo

  local idx
  while true; do
    read -rp "  Choose branch [1-${#all[@]}]: " idx
    if [[ "$idx" =~ ^[0-9]+$ ]] && (( idx >= 1 && idx <= ${#all[@]} )); then
      break
    fi
    warn "Enter a number between 1 and ${#all[@]}."
  done

  local target="${all[$((idx - 1))]}"
  git checkout "$target"
  echo
  success "Switched to ${BOLD}${target}${RESET}"
  info "Commits here produce ${GREEN}patch releases${RESET} (${target#release/}.N) via CI."
  echo
}

# ── cmd: backport ─────────────────────────────────────────────────────────────
# Copies commits made on main onto one or more release/X.Y lines with
# `git cherry-pick -x`. Each line then publishes them as a patch release: the
# commit-analyzer caps feat and breaking commits at patch on release/* branches.
#
# All bookkeeping lives in plain files under a temp dir: this script must keep
# running on macOS's bash 3.2, which has no associative arrays.

# The subjects of the commits this script writes. Every `git commit` here builds
# its message from these, so _is_release_machinery cannot fall out of step. They
# must stay byte for byte as they are: commits already in projects' histories
# carry them. MSG_RELEASE is the one this script does not write: it is the
# @semantic-release/git "message" in template/releaserc.json.tmpl (the seed
# commit of cmd_bootstrap reuses it on purpose).
readonly MSG_RELEASE="chore(release): "
readonly MSG_ADD_WORKFLOWS="ci: add workflow files"
readonly MSG_REGISTER="ci: register "
readonly MSG_REGISTER_TAIL=" in semantic-release branches"
readonly MSG_ADVANCE="chore: advance main past "
readonly MSG_SYNC="ci: sync workflow files"
readonly MSG_REGEN="ci: regenerate semantic-release branches"
readonly MSG_TRIGGER="chore: trigger CI to graduate to "
readonly MSG_PRE_TRIGGER="chore(pre-release): trigger "

# _is_release_machinery <subject>: true for the subjects that release tooling
# writes, never a developer. Copying one onto a line would overwrite its
# VERSION.txt, CHANGELOG.md or .releaserc.json with another line's.
_is_release_machinery() {
  local p
  for p in "$MSG_RELEASE" "$MSG_ADD_WORKFLOWS" "$MSG_ADVANCE" "$MSG_SYNC" "$MSG_REGEN" "$MSG_TRIGGER" "$MSG_PRE_TRIGGER"; do
    [[ "$1" == "$p"* ]] && return 0
  done
  [[ "$1" == "$MSG_REGISTER"*"$MSG_REGISTER_TAIL"* ]]
}

# _branch_ref <release/X.Y>: the ref that best describes the line. The local
# branch, unless origin is strictly ahead of it (fetched but not yet pulled).
_branch_ref() {
  local l="refs/heads/$1" r="refs/remotes/origin/$1"
  if git show-ref --verify --quiet "$l"; then
    if git show-ref --verify --quiet "$r" && [[ "$(git rev-parse "$l")" != "$(git rev-parse "$r")" ]] \
       && git merge-base --is-ancestor "$l" "$r"; then
      echo "$r"
    else
      echo "$l"
    fi
  else
    echo "$r"
  fi
}

# _line_refs <release/X.Y>: print the local and origin refs of the line that
# exist, one per line. For questions like "does the line carry commit C",
# where either copy counts.
_line_refs() {
  local r
  for r in "refs/heads/$1" "refs/remotes/origin/$1"; do
    git show-ref --verify --quiet "$r" && echo "$r"
  done
  return 0
}

# _release_only_commits <dir>: write <dir>/release-only, the commits made on any
# release line after it forked from main, and <dir>/fork-<X.Y>, each fork point.
# finalize ours-merges each line's graduation commit into main, so main's ancestry
# also holds what the line did before graduating (its `ci:` commits, and, with
# older finalizes that merged the whole branch, its patches and picked copies).
# Offering those as "main commits" would backport from one line to another.
# The fork is the newest commit of the line's first-parent chain that is also on
# main's. Whole-history --first-parent was not an option: it would hide commits
# that reach main through PR merges.
_release_only_commits() {
  local d="$1" xy ref fork
  local -a refs
  : > "$d/release-only"
  git rev-list --first-parent main > "$d/main-fp"
  while IFS= read -r xy; do
    [[ -n "$xy" ]] || continue
    ref=$(_branch_ref "release/$xy")
    git rev-list --first-parent "$ref" > "$d/line-fp"
    # File against file: `rev-list | grep -m1` would SIGPIPE under pipefail.
    fork=$(grep -m1 -xFf "$d/main-fp" "$d/line-fp" || true)
    if [[ -z "$fork" ]]; then
      warn "Cannot find where ${CYAN}release/${xy}${RESET} forked from main. Its own commits are not filtered."
      : > "$d/fork-$xy"
      continue
    fi
    echo "$fork" > "$d/fork-$xy"
    refs=()
    git show-ref --verify --quiet "refs/heads/release/$xy"          && refs+=("refs/heads/release/$xy")
    git show-ref --verify --quiet "refs/remotes/origin/release/$xy" && refs+=("refs/remotes/origin/release/$xy")
    git rev-list "${refs[@]}" "^$fork" >> "$d/release-only"
  done <<< "$(release_lines)"
}

# _missing_on <ref> <X.Y> <dir>: print "sha<TAB>subject" for every non-merge
# commit of main that <ref> lacks, newest first. A commit counts as present when
# the line holds a patch-identical copy (git cherry semantics, so hand-made copies
# count too) or a commit whose `-x` trailer names it, which is the only trace a
# pick leaves once its conflict was resolved. Release machinery, empty commits
# and commits made on release lines (see _release_only_commits) are never listed.
_missing_on() {
  local ref="$1" xy="$2" d="$3" fork sha subject
  fork=$(cat "$d/fork-$xy" 2>/dev/null || true)
  cp "$d/release-only" "$d/exclude"
  if [[ -n "$fork" ]]; then
    git log --format=%B "${fork}..${ref}" \
      | sed -n 's/^(cherry picked from commit \([0-9a-f]*\))$/\1/p' >> "$d/exclude"
  fi
  git log --no-merges --right-only --cherry-pick --format='%H%x09%s' "${ref}...main" \
    | _rows_not_in "$d/exclude" - \
    | while IFS=$'\t' read -r sha subject; do
        _is_release_machinery "$subject" || _is_empty_commit "$sha" \
          || printf '%s\t%s\n' "$sha" "$subject"
      done
}

# _is_empty_commit <sha>: true when <sha> changes nothing, like the marker that
# cmd_major writes with --allow-empty. `git cherry-pick` refuses such a commit,
# and a backport would stop on it as if it were a conflict. A root commit is
# never reported empty.
_is_empty_commit() {
  local t
  t=$(git rev-parse "$1^{tree}" "$1^^{tree}" 2>/dev/null) || return 1
  [[ "${t%%$'\n'*}" == "${t#*$'\n'}" ]]
}

# _rows_in <keys> <rows|->: print the rows whose first tab-separated column is
# listed in <keys>, in the order of <rows>. _rows_not_in prints the others.
_rows_in()     { awk -F'\t' 'FILENAME == ARGV[1] { k[$1] = 1; next } ($1 in k)'  "$1" "$2"; }
_rows_not_in() { awk -F'\t' 'FILENAME == ARGV[1] { k[$1] = 1; next } !($1 in k)' "$1" "$2"; }

# _in_main_order <dir> <file>: print the SHAs listed in <file> (first column)
# newest first, in main's topological order. Pipe through _reverse for oldest first.
_in_main_order() { _rows_in "$2" "$1/main-order"; }
_reverse() { awk '{ l[NR] = $0 } END { for (i = NR; i > 0; i--) print l[i] }'; }

# _parse_selection <max> <input>: print the 1-based indices chosen by input such
# as "1,3-5", or "a" for all, one per line and sorted. Non-zero when the input is
# malformed or out of range.
_parse_selection() {
  local max="$1" input="${2// /}" part lo hi i out=""
  local -a parts
  [[ -n "$input" ]] || return 1
  if [[ "$input" == [aA] ]]; then input="1-${max}"; fi
  IFS=',' read -r -a parts <<< "$input"
  for part in "${parts[@]+"${parts[@]}"}"; do
    if [[ "$part" =~ ^([0-9]+)-([0-9]+)$ ]]; then
      lo=$((10#${BASH_REMATCH[1]})); hi=$((10#${BASH_REMATCH[2]}))
    elif [[ "$part" =~ ^[0-9]+$ ]]; then
      lo=$((10#$part)); hi=$lo
    else
      return 1
    fi
    (( lo >= 1 && lo <= hi && hi <= max )) || return 1
    for (( i = lo; i <= hi; i++ )); do out+="${i}"$'\n'; done
  done
  printf '%s' "$out" | sort -n -u
}

# _ask_selection <prompt> <max>: re-prompt until _parse_selection accepts the
# answer, then print its indices. Messages go to stderr, since callers capture
# stdout. Command substitution does not inherit `set -e`, hence the explicit die.
_ask_selection() {
  local prompt="$1" max="$2" input out
  while true; do
    read -rp "  ${prompt}: " input || die "No answer on stdin."
    if out=$(_parse_selection "$max" "$input"); then
      echo "$out"
      return 0
    fi
    echo -e "  ${YELLOW}⚠${RESET}  Enter numbers between 1 and ${max} (like 1,3-4), or a for all." >&2
  done
}

cmd_backport() {
  header "Backport Commits from main"
  rule

  require_clean_tree
  git show-ref --verify --quiet refs/heads/main || die "No local ${BOLD}main${RESET} branch to backport from."
  # A detached HEAD has no branch name to return to (current_branch would say
  # "HEAD", and checking that out goes nowhere), so remember the commit.
  local start
  start=$(git symbolic-ref -q --short HEAD || git rev-parse HEAD)

  info "Fetching origin…"
  git fetch --quiet origin 2>/dev/null || warn "Could not reach origin. Proceeding with local refs."
  if git show-ref --verify --quiet refs/remotes/origin/main; then
    local behind
    behind=$(git rev-list --count main..origin/main)
    if (( behind > 0 )); then
      warn "Local main is ${behind} commit(s) behind origin/main. Pull it to backport those too."
    fi
  fi

  local lines
  lines=$(release_lines)
  [[ -n "$lines" ]] || die "No release/* branches found. Run ${BOLD}${SELF} bootstrap${RESET} first."

  local d
  d=$(mktemp -d)
  # shellcheck disable=SC2064 # expand now: $d is local and gone by EXIT time
  trap "rm -rf '$d'" EXIT

  # ── targets ──────────────────────────────────────────────────────────────
  local -a all=() targets=()
  local xy i highest
  while IFS= read -r xy; do all+=("$xy"); done <<< "$lines"
  highest="${all[${#all[@]}-1]}"

  echo
  echo -e "  Release lines:\n"
  for i in "${!all[@]}"; do
    if [[ "${all[$i]}" == "$highest" ]]; then
      echo -e "  $(printf '%2d' $((i + 1))))  ${CYAN}release/${all[$i]}${RESET}  ${GREEN}latest${RESET}"
    else
      echo -e "  $(printf '%2d' $((i + 1))))  ${CYAN}release/${all[$i]}${RESET}  maintenance"
    fi
  done
  echo
  local sel
  sel=$(_ask_selection "Backport to which lines? (e.g. 1,2 or a)" "${#all[@]}")
  while IFS= read -r i; do targets+=("${all[$((i - 1))]}"); done <<< "$sel"

  git rev-list --topo-order --no-merges main > "$d/main-order"
  _release_only_commits "$d"
  for xy in "${targets[@]}"; do
    _missing_on "$(_branch_ref "release/$xy")" "$xy" "$d" > "$d/miss-$xy"
  done

  # ── commits ──────────────────────────────────────────────────────────────
  : > "$d/chosen"
  local arg sha subject
  if (( $# > 0 )); then
    for arg in "$@"; do
      sha=$(git rev-parse --verify --quiet "${arg}^{commit}") || die "Unknown commit '${arg}'."
      git merge-base --is-ancestor "$sha" main || die "${arg} is not on main."
      if grep -qxF "$sha" "$d/release-only"; then
        die "${arg} was made on a release branch, not on main. Backport only copies main's commits."
      fi
      [[ $(git rev-list --parents -n1 "$sha" | wc -w) -le 2 ]] || die "${arg} is a merge commit. Backport its parents instead."
      subject=$(git log -1 --format=%s "$sha")
      _is_release_machinery "$subject" && die "${arg} (${subject}) is release machinery and must not be backported."
      _is_empty_commit "$sha" && die "${arg} (${subject}) changes nothing, so there is nothing to backport."
      echo "$sha" >> "$d/chosen"
    done
  else
    cat "$d"/miss-* | cut -f1 | sort -u > "$d/union"
    _in_main_order "$d" "$d/union" > "$d/candidates"
    if [[ ! -s "$d/candidates" ]]; then
      success "Nothing to backport: the selected lines already have every commit of main."
      echo
      return
    fi
    local total shown=0 where
    total=$(wc -l < "$d/candidates" | tr -d ' ')
    echo
    echo -e "  Commits of ${BOLD}main${RESET} missing on the selected lines (newest first):\n"
    while IFS= read -r sha && (( shown < 40 )); do
      shown=$((shown + 1))
      where=""
      if (( ${#targets[@]} > 1 )); then
        for xy in "${targets[@]}"; do
          if awk -F'\t' -v s="$sha" '$1 == s { f = 1 } END { exit !f }' "$d/miss-$xy"; then
            where+="${where:+, }${xy}"
          fi
        done
        where="  ${DIM}(missing on ${where})${RESET}"
      fi
      echo -e "  $(printf '%2d' "$shown"))  ${YELLOW}$(git rev-parse --short "$sha")${RESET}  $(git log -1 --format=%s "$sha")${where}"
      echo "$sha" >> "$d/menu"
    done < "$d/candidates"
    if (( total > shown )); then
      echo
      info "$((total - shown)) older commit(s) not shown. Pass their SHAs as arguments to backport them."
    fi
    echo
    sel=$(_ask_selection "Commits to backport (e.g. 1,3-5 or a)" "$shown")
    while IFS= read -r i; do sed -n "${i}p" "$d/menu" >> "$d/chosen"; done <<< "$sel"
  fi
  # Apply oldest first, whatever order the SHAs were typed in.
  _in_main_order "$d" "$d/chosen" | _reverse > "$d/ordered"

  # ── summary ──────────────────────────────────────────────────────────────
  rule
  local n any=0
  for xy in "${targets[@]}"; do
    _rows_in "$d/miss-$xy" "$d/ordered" > "$d/plan-$xy"
    n=$(wc -l < "$d/plan-$xy" | tr -d ' ')
    echo -e "  ${CYAN}release/${xy}${RESET}:"
    if (( n == 0 )); then
      echo -e "    ${DIM}already has every chosen commit${RESET}"
    else
      any=1
      while IFS= read -r sha; do
        echo -e "    ${YELLOW}$(git rev-parse --short "$sha")${RESET}  $(git log -1 --format=%s "$sha")"
      done < "$d/plan-$xy"
    fi
  done
  echo
  info "On a release line, ${BOLD}feat${RESET} and breaking commits publish as ${GREEN}patch${RESET} releases."
  rule
  if (( any )); then
    ask_yes "Apply?" || { info "Aborted. Nothing changed."; echo; return; }
  fi

  # ── apply ────────────────────────────────────────────────────────────────
  local b ab
  local -a picks done_lines=() stale=()
  for xy in "${targets[@]}"; do
    b="release/$xy"
    git checkout --quiet "$b"
    if git show-ref --verify --quiet "refs/remotes/origin/$b"; then
      if ! git merge --ff-only --quiet "origin/$b" 2>/dev/null; then
        git checkout --quiet "$start"
        die "${b} has diverged from origin/${b}. Reconcile it, then run backport again."
      fi
    fi
    _releaserc_in_sync || stale+=("$xy")

    _missing_on HEAD "$xy" "$d" > "$d/miss-now"
    picks=()
    while IFS= read -r sha; do picks+=("$sha"); done \
      < <(_rows_in "$d/miss-now" "$d/ordered")

    if (( ${#picks[@]} == 0 )); then
      info "${b} already has every chosen commit."
      done_lines+=("$xy")
      continue
    fi

    # One cherry-pick call, so after a conflict git's sequencer can --continue the rest.
    if ! git cherry-pick -x "${picks[@]}"; then
      echo
      warn "Cherry-pick stopped on ${BOLD}${b}${RESET} with a conflict."
      echo -e "  Resolve it, then:     ${BOLD}git add <files> && git cherry-pick --continue${RESET}"
      echo -e "  If the pick is empty: ${BOLD}git cherry-pick --skip${RESET}   ${DIM}(the change is already there)${RESET}"
      echo -e "  To give up here:      ${BOLD}git cherry-pick --abort${RESET}"
      echo
      if (( ${#done_lines[@]} > 0 )); then
        echo -e "  Already applied, not pushed: ${CYAN}${done_lines[*]}${RESET}"
      fi
      local -a rest=()
      ab=0
      for i in "${targets[@]}"; do
        (( ab )) && rest+=("$i")
        [[ "$i" == "$xy" ]] && ab=1
      done
      if (( ${#rest[@]} > 0 )); then
        echo -e "  Not started yet:             ${CYAN}${rest[*]}${RESET}"
      fi
      local shas stopped rest_shas
      shas=$(while IFS= read -r sha; do git rev-parse --short "$sha"; done < "$d/ordered" | tr '\n' ' ')
      echo
      echo -e "  Then finish and push all of them with the same lines selected:"
      echo -e "    ${BOLD}${SELF} backport ${shas% }${RESET}"
      # A skipped pick leaves nothing on the line (no commit, so no -x trailer),
      # and the command above would pick it again and stop again. Name it.
      stopped=$(git rev-parse -q --verify CHERRY_PICK_HEAD 2>/dev/null || true)
      if [[ -n "$stopped" ]]; then
        rest_shas=$(while IFS= read -r sha; do
                      [[ "$sha" == "$stopped" ]] || git rev-parse --short "$sha"
                    done < "$d/ordered" | tr '\n' ' ')
        echo -e "  If you skipped ${YELLOW}$(git rev-parse --short "$stopped")${RESET}, leave it out:"
        if [[ -n "$rest_shas" ]]; then
          echo -e "    ${BOLD}${SELF} backport ${rest_shas% }${RESET}"
        else
          echo -e "    ${DIM}(nothing else to pick; push the finished lines with git push origin <branch>)${RESET}"
        fi
        echo -e "  ${DIM}A backport without arguments will keep offering it: nothing on the line records the skip.${RESET}"
      fi
      echo
      exit 1
    fi
    success "Applied ${#picks[@]} commit(s) to ${BOLD}${b}${RESET}"
    done_lines+=("$xy")
  done
  git checkout --quiet "$start"
  echo

  # ── push ─────────────────────────────────────────────────────────────────
  # Every selected line that is ahead of origin, whatever this run did: a re-run
  # after a conflict must still push the lines finished the first time.
  local -a push=()
  local s is_stale
  for xy in "${targets[@]}"; do
    b="release/$xy"
    git show-ref --verify --quiet "refs/heads/$b" || continue
    if git show-ref --verify --quiet "refs/remotes/origin/$b" \
       && [[ $(git rev-list --count "origin/$b..$b") -eq 0 ]]; then
      continue
    fi
    is_stale=0
    for s in "${stale[@]+"${stale[@]}"}"; do [[ "$s" == "$xy" ]] && is_stale=1; done
    if (( is_stale )); then
      warn "${CYAN}${b}${RESET} carries a ${RELEASERC} that is out of sync with the release branches."
      echo -e "  Its patch may publish on the wrong channel. Fix:  ${BOLD}${SELF} switch${RESET}  →  ${BOLD}${SELF} sync${RESET}"
      ask_yes "Push ${b} anyway?" || { info "Leaving ${b} unpushed."; continue; }
    fi
    push+=("$b")
  done

  if (( ${#push[@]} == 0 )); then
    success "Nothing to push."
    echo
    return
  fi

  echo -e "  Ready to push:\n"
  for b in "${push[@]}"; do
    echo -e "  ${CYAN}${b}${RESET}"
    if git show-ref --verify --quiet "refs/remotes/origin/$b"; then
      git --no-pager log --oneline "origin/$b..$b" | sed 's/^/    /'
    else
      echo -e "    ${DIM}(new on origin)${RESET}"
    fi
  done
  echo
  info "Each push makes ${CI_NAME} publish a ${GREEN}patch${RESET} release on that line."
  if ask_no "Push ${#push[@]} branch(es) now?"; then
    for b in "${push[@]}"; do
      if git show-ref --verify --quiet "refs/remotes/origin/$b"; then
        git push origin "$b"
      else
        git push -u origin "$b"
      fi
      success "Pushed ${b}"
    done
  else
    warn "Not pushed yet. Push with ${BOLD}git push origin <branch>${RESET}, or undo with:"
    for b in "${push[@]}"; do
      git show-ref --verify --quiet "refs/remotes/origin/$b" || continue
      if [[ "$b" == "$start" ]]; then
        echo -e "    ${BOLD}git reset --hard origin/${b}${RESET}"
      else
        echo -e "    ${BOLD}git branch -f ${b} origin/${b}${RESET}"
      fi
    done
  fi
  echo
}

# ── cmd: branches ──────────────────────────────────────────────────────────
# Regenerate .releaserc.json's "branches" array from the actual release/* refs
# and commit it on the current branch. Idempotent — run it on main or on any
# release branch to bring that branch's view of the topology back in sync.
cmd_branches() {
  header "Regenerate semantic-release branches"
  rule

  local branch
  branch=$(current_branch)
  require_clean_tree

  local lines highest
  lines=$(release_lines)
  [[ -n "$lines" ]] || die "No release/* branches found. Run ${BOLD}${SELF} bootstrap${RESET} first."
  highest=$(echo "$lines" | tail -1)

  echo -e "  Rebuilding the ${CYAN}branches${RESET} array of ${BOLD}${RELEASERC}${RESET} from git refs:\n"
  while IFS= read -r xy; do
    if [[ "$xy" == "$highest" ]]; then
      printf "    ${CYAN}release/%s${RESET}  →  ${GREEN}latest${RESET}  ${DIM}(current release)${RESET}\n" "$xy"
    else
      printf "    ${CYAN}release/%s${RESET}  →  maintenance  ${DIM}(range %s.x, channel %s.x)${RESET}\n" "$xy" "$xy" "$xy"
    fi
  done <<< "$lines"
  echo -e "    ${YELLOW}main${RESET}        →  ${YELLOW}alpha${RESET} prerelease  ${DIM}(pre)${RESET}"
  rule

  regenerate_branches
  if git diff --quiet -- "$RELEASERC"; then
    success "${RELEASERC} already up to date on ${BOLD}${branch}${RESET}."
    echo
    return
  fi

  git --no-pager diff -- "$RELEASERC"
  echo
  if ! ask_yes "Commit this ${RELEASERC} on ${BOLD}${branch}${RESET}?"; then
    git checkout -- "$RELEASERC"
    info "Reverted — nothing committed."
    echo
    return
  fi

  git add "$RELEASERC"
  git commit -m "${MSG_REGEN} [skip ci]" >/dev/null
  success "Committed updated ${RELEASERC} on ${BOLD}${branch}${RESET}."
  echo

  if ask_no "Push now?"; then
    git push
    success "Pushed."
  else
    warn "Not pushed yet."
    echo -e "  Push when ready:  ${BOLD}git push${RESET}"
  fi
  echo
}

# ── cmd: major ────────────────────────────────────────────────────────────────
# Creates a BREAKING CHANGE commit on main, signalling semantic-release to
# bump the MAJOR version on the next pre-release cycle.
cmd_major() {
  header "Force Major Version Bump"
  rule

  require_branch main

  # If nothing is staged, offer to stage everything (mirrors cmd_commit).
  if git diff --cached --quiet; then
    local unstaged
    unstaged=$(git diff --name-only)
    if [[ -n "$unstaged" ]]; then
      warn "Nothing is staged. Unstaged files:"
      while IFS= read -r f; do echo -e "    ${DIM}${f}${RESET}"; done <<< "$unstaged"
      echo
      read -rp "  Stage all changes now (git add -A)? [y/N] " stage_all
      [[ "$stage_all" =~ ^[Yy]$ ]] && { git add -A; success "Staged all changes."; }
    fi
    # Proceed anyway — git commit --allow-empty handles a clean tree.
  fi

  local version next_major
  version=$(current_version)
  if [[ "$version" =~ ^([0-9]+)\. ]]; then
    next_major=$(( BASH_REMATCH[1] + 1 ))
    printf "  %-28s %s\n" "Current version:"  "${YELLOW}${version}${RESET}"
    printf "  %-28s %s\n" "After push + CI:"  "${YELLOW}${next_major}.0.0-pre.1${RESET}"
    printf "  %-28s %s\n" "After cut + CI:"   "${GREEN}${next_major}.0.0${RESET}"
    rule
  fi

  echo
  read -rp "  Short description: " description
  [[ -n "$description" ]] || die "Description cannot be empty."

  echo
  read -rp "  Breaking change description (for CHANGELOG): " breaking_desc
  [[ -n "$breaking_desc" ]] || die "Breaking change description cannot be empty."

  echo
  read -rp "  Extended body (optional — Enter to skip): " body

  local subject="feat!: ${description}"
  local msg="$subject"
  [[ -n "$body" ]] && msg+=$'\n\n'"$body"
  msg+=$'\n\n'"BREAKING CHANGE: ${breaking_desc}"

  rule
  echo -e "  ${BOLD}Preview:${RESET}\n"
  echo -e "  ${BOLD}${subject}${RESET}"
  [[ -n "$body" ]] && echo -e "\n  ${body}"
  echo -e "\n  ${RED}BREAKING CHANGE: ${breaking_desc}${RESET}"
  echo
  rule

  read -rp "  Commit? [Y/n] " confirm
  [[ "$confirm" =~ ^[Nn]$ ]] && { info "Aborted — nothing committed."; return; }

  git commit --allow-empty -m "$msg"
  echo
  success "Committed:  ${BOLD}${subject}${RESET}"
  info "Push to origin to trigger the ${BOLD}major${RESET} pre-release."
  info "Then run ${BOLD}${SELF} cut${RESET} and ${BOLD}${SELF} finalize${RESET} as usual."
  echo
}

# ── cmd: release ──────────────────────────────────────────────────────────────
# Run semantic-release locally instead of waiting for CI.
#
# Outside CI, semantic-release silently turns itself into a dry run: it
# detects no CI environment and publishes nothing. --no-ci is passed through
# to semantic-release, which then really tags, pushes and publishes.
cmd_release() {
  local -a sr_args=()
  while (( $# )); do
    case "$1" in
      --no-ci) sr_args+=(--no-ci) ;;
      *) die "Unknown option '${1}' for release. Usage: ${BOLD}${SELF} release [--no-ci]${RESET}" ;;
    esac
    shift
  done

  header "Manual Release"
  rule

  local branch
  branch=$(current_branch)

  case "$branch" in
    main|release/*) ;;
    *) die "Must be on ${BOLD}main${RESET} or a ${BOLD}release/*${RESET} branch (currently on '${BOLD}${branch}${RESET}')." ;;
  esac

  require_clean_tree

  # ── prerequisites ──────────────────────────────────────────────────────
  command -v node >/dev/null 2>&1 || die "Node.js is required but not installed."
  command -v npx  >/dev/null 2>&1 || die "npx is required but not installed."

  if [[ ! -d node_modules ]]; then
    die "node_modules/ not found. Run ${BOLD}npm ci${RESET} first."
  fi

  if ! npx --no-install semantic-release --version >/dev/null 2>&1; then
    warn "semantic-release not in node_modules — npx will auto-install it."
    echo
  fi

  # Check for workflow file on release branches — manual release is the
  # only path if there is none.
  if [[ "$branch" == release/* ]]; then
    local root
    root=$(git rev-parse --show-toplevel)
    if [[ ! -f "${root}/${CI_DIR}/workflows/release.yml" ]]; then
      warn "No workflow file at ${CI_DIR}/workflows/release.yml — manual release is the only path here."
      echo
    fi
  fi

  # ── identity ───────────────────────────────────────────────────────────
  : "${GIT_AUTHOR_NAME:=$(git config user.name 2>/dev/null || echo "")}"
  : "${GIT_AUTHOR_EMAIL:=$(git config user.email 2>/dev/null || echo "")}"
  : "${GIT_COMMITTER_NAME:=${GIT_AUTHOR_NAME}}"
  : "${GIT_COMMITTER_EMAIL:=${GIT_AUTHOR_EMAIL}}"
  export GIT_AUTHOR_NAME GIT_AUTHOR_EMAIL GIT_COMMITTER_NAME GIT_COMMITTER_EMAIL

  if [[ -z "$GIT_AUTHOR_NAME" || -z "$GIT_AUTHOR_EMAIL" ]]; then
    die "Git identity not set. Run ${BOLD}git config user.name${RESET} / ${BOLD}git config user.email${RESET}, or export GIT_AUTHOR_NAME / GIT_AUTHOR_EMAIL."
  fi

  # ── forge token ────────────────────────────────────────────────────────
  # Git push uses existing credentials (SSH, credential helper, etc.).  The
  # forge API call (GitHub/Gitea release) needs a token — warn early rather
  # than failing deep inside npx.
  local token_var
  token_var=$(_token_var_for_ci)
  if [[ -n "$token_var" ]] && [[ -z "${!token_var:-}" ]]; then
    warn "${token_var} is not set. Git push will work but the forge release (${CI_NAME}) will fail."
    echo
    ask_no "Continue without ${token_var}?" || { info "Aborted.  ${BOLD}export ${token_var}=your_token${RESET}"; echo; return; }
  fi

  # ── verify .releaserc.json is in sync with release branches ────────────
  if ! _releaserc_in_sync; then
    warn "${RELEASERC} branches are out of sync with git refs."
    info "Run ${BOLD}${SELF} branches${RESET} to fix, or continue with stale config."
    echo
    ask_no "Continue with stale ${RELEASERC}?" || { info "Aborted."; echo; return; }
  fi

  # ── run ────────────────────────────────────────────────────────────────
  echo
  info "Running: npx semantic-release ${sr_args[*]}"
  info "Branch:  ${BOLD}${branch}${RESET}"
  if (( ${#sr_args[@]} == 0 )) && [[ -z "${CI:-}" ]]; then
    warn "Not in CI, so semantic-release will only do a dry run."
    info "Pass ${BOLD}--no-ci${RESET} to publish for real: ${BOLD}${SELF} release --no-ci${RESET}"
  fi
  if [[ -n "${token_var:-}" ]] && [[ -n "${!token_var:-}" ]]; then
    info "Token:   ${token_var} is set"
  fi
  echo
  rule

  local rc=0
  npx semantic-release "${sr_args[@]}" || rc=$?
  echo

  if [[ $rc -eq 0 ]] && (( ${#sr_args[@]} == 0 )) && [[ -z "${CI:-}" ]]; then
    success "Dry run completed. Nothing was tagged or published."
    info "Run ${BOLD}${SELF} release --no-ci${RESET} to publish."
    echo
  elif [[ $rc -eq 0 ]]; then
    success "Release completed."
    info "Tag and release commit are on ${BOLD}${branch}${RESET}."
    echo
  else
    die "semantic-release exited with code ${rc}."
  fi
}

# ── cmd: help ────────────────────────────────────────────────────────────────
cmd_help() {
  echo -e "${BOLD}release.sh${RESET} — semver release workflow helper"
  rule
  echo -e "  Usage: ${BOLD}${SELF} <command>${RESET}"
  echo
  local -a _cmds _descs
  _cmds=(bootstrap finalize sync branches status commit major cut graduate release switch backport help)
  _descs=(
    "${BOLD}One-time setup${RESET}: create initial release/X.Y branch so CI works"
    "After CI graduates X.Y.0, merge it back so main advances"
    "Copy workflow files + ${RELEASERC} from main onto current release branch"
    "Regenerate ${RELEASERC} branches (latest + maintenance) from git refs"
    "Show current branch, version, and what comes next"
    "Create a conventional commit interactively"
    "Create a ${RED}BREAKING CHANGE${RESET} commit to force a MAJOR version bump"
    "Cut a release/X.Y branch from main → CI produces X.Y.0"
    "Force X.Y.0 (on release/X.Y) or the next pre-release (on main) with an empty commit"
    "Run semantic-release locally (${BOLD}--no-ci${RESET} to publish, not dry-run)"
    "Switch to an existing release/X.Y branch for patch work"
    "Copy chosen commits from main onto release/X.Y lines (cherry-pick -x)"
    "Show this help"
  )
  local _w=0
  for _c in "${_cmds[@]}"; do (( ${#_c} > _w )) && _w=${#_c}; done
  for _i in "${!_cmds[@]}"; do
    printf "  ${CYAN}%-${_w}s${RESET}  %s\n" "${_cmds[$_i]}" "${_descs[$_i]}"
  done
  rule
  echo -e "  ${BOLD}First-time setup (do this once):${RESET}"
  echo
  echo -e "    ${DIM}${SELF} bootstrap${RESET}  →  creates ${CYAN}release/X.Y${RESET} from last tag"
  echo -e "                                      pushes it  →  CI on main now passes"
  echo
  rule
  echo -e "  ${BOLD}Ongoing workflow (with CI):${RESET}"
  echo
  echo -e "    1. Work on ${YELLOW}main${RESET}"
  echo -e "       ${DIM}${SELF} commit${RESET}  →  push  →  CI tags ${YELLOW}X.Y.0-pre.N${RESET}"
  echo
  echo -e "    2. Ready to ship"
  echo -e "       ${DIM}${SELF} cut${RESET}      →  CI tags ${GREEN}X.Y.0${RESET} (graduation); cut waits for it"
  echo -e "       ${DIM}${SELF} finalize${RESET}  →  main now targets ${YELLOW}X.Y+1.0-pre.1${RESET} (cut offers it)"
  echo
  echo -e "    3. Patch fixes"
  echo -e "       ${DIM}${SELF} switch${RESET}  →  ${DIM}commit${RESET}  →  push  →  CI tags ${GREEN}X.Y.Z${RESET}"
  echo -e "       or fix on main, then ${DIM}${SELF} backport${RESET}  →  pick commits and lines  →  push"
  echo
  local _tv
  _tv=$(_token_var_for_ci)
  rule
  echo -e "  ${BOLD}Manual workflow (no CI):${RESET}"
  echo
  echo -e "    Run ${DIM}${SELF} release --no-ci${RESET} on the target branch instead of pushing."
  echo -e "    Without ${BOLD}--no-ci${RESET}, semantic-release detects no CI and only does a dry run."
  echo -e "    Requires ${BOLD}node_modules/${RESET} with semantic-release installed."
  echo -e "    Git push uses your existing credentials; set ${CYAN}${_tv}${RESET} for forge API."
  echo
  rule
  echo -e "  ${BOLD}Maintenance model:${RESET}"
  echo
  echo -e "    The highest ${CYAN}release/X.Y${RESET} line owns the ${GREEN}latest${RESET} channel; older lines are"
  echo -e "    ${BOLD}maintenance${RESET} branches (range ${DIM}X.Y.x${RESET}, channel ${DIM}X.Y.x${RESET}) and do NOT count"
  echo -e "    toward semantic-release's 3-release-branch limit. ${DIM}${SELF} cut${RESET} regenerates"
  echo -e "    ${CYAN}${RELEASERC}${RESET} from the git refs; ${DIM}${SELF} branches${RESET} re-syncs it on any branch."
  echo
}

# ── dispatch ──────────────────────────────────────────────────────────────────
case "${1:-help}" in
  bootstrap)    cmd_bootstrap ;;
  finalize)     cmd_finalize  ;;
  sync)         cmd_sync      ;;
  branches)     cmd_branches  ;;
  status)       cmd_status    ;;
  commit)       cmd_commit    ;;
  major)        cmd_major     ;;
  cut)          cmd_cut       ;;
  graduate)     shift; cmd_graduate "$@" ;;
  release|publish) shift; cmd_release "$@" ;;
  switch)       cmd_switch    ;;
  backport)     shift; cmd_backport "$@" ;;
  help|--help|-h) cmd_help   ;;
  *) die "Unknown command '${1}'. Run ${BOLD}${SELF} help${RESET} for usage." ;;
esac
