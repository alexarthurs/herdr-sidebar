#!/bin/bash
# A persistent playground repo exercising every Source Control feature:
# branches, a worktree, stashes, tags, two remotes, staged/unstaged/untracked.
set -e
APP=/c/Users/Alex/Projects/scm-playground
WT=/c/Users/Alex/Projects/scm-playground-search
ORIGIN=/c/Users/Alex/Projects/.scm-playground-origin.git
G="git -c user.name=Alex_Arthurs -c user.email=alex@example.com"
rm -rf "$APP" "$WT" "$ORIGIN"
mkdir -p "$APP/src" "$APP/docs"
cd "$APP"
git init -q -b main

cat > src/app.ts <<'EOF'
import { search } from "./search";

export function main(): void {
  console.log("scm-playground up");
  search("hello");
}
EOF
cat > src/search.ts <<'EOF'
export function search(query: string): string[] {
  if (!query) return [];
  return [query.toLowerCase()];
}
EOF
cat > README.md <<'EOF'
# scm-playground

A sandbox repo for exercising the herdr sidebar's Source Control view:
branches, worktrees, stashes, tags, remotes — break anything you like.
EOF
printf '/node_modules\n/dist\n' > .gitignore
git add -A && $G commit -qm "Scaffold app and search module"

cat > docs/usage.md <<'EOF'
# Usage

Run `main()` and watch the search results roll in.
EOF
git add docs && $G commit -qm "Add usage docs"

printf 'export const VERSION = "0.1.0";\n' > src/version.ts
git add src && $G commit -qm "Introduce version constant"
git tag v0.1.0

# Bare origin here; later commits leave main 1 ahead.
git clone -q --bare . "$ORIGIN"
git remote add origin "$ORIGIN"
git remote add github https://github.com/alexarthurs/scm-playground.git
git fetch -q origin
git branch -q --set-upstream-to=origin/main main

printf 'export const VERSION = "0.2.0";\n' > src/version.ts
git add src && $G commit -qm "Bump version to 0.2.0"
$G tag -a v0.2.0 -m "Second release"

# Branches + a worktree checked out on one of them.
git branch feature/search
git branch fix/crash-on-load
git worktree add "$WT" feature/search >/dev/null

# Two stashes with distinct messages.
printf '\n// tuning pass\n' >> src/search.ts
$G stash push -qm "wip: fuzzy matching experiment"
printf '\n## FAQ\n' >> docs/usage.md
$G stash push -qm "wip: usage faq"

# Working-tree spread: staged, modified, untracked.
cat > docs/roadmap.md <<'EOF'
# Roadmap

- [ ] fuzzy search
- [ ] result ranking
EOF
git add docs/roadmap.md
printf '\n// TODO: debounce input\n' >> src/app.ts
printf 'console.log("scratch");\n' > scratch.js

echo "--- status:"; git status --short --branch
echo "--- stashes:"; git stash list
echo "--- tags:"; git tag
echo "--- worktrees:"; git worktree list
echo "--- remotes:"; git remote -v | grep fetch
