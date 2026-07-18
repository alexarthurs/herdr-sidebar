#!/bin/bash
# Recreate the acme-app demo repo used for screenshots.
set -e
APP=$HOME/Projects/acme-app
ORIGIN=$HOME/Projects/.acme-origin.git
rm -rf "$APP" "$ORIGIN"
mkdir -p "$APP/src/api" "$APP/docs"
cd "$APP"
git init -q -b main

cat > src/api/routes.rs <<'EOF'
use std::collections::HashMap;

/// Route table for the public API.
pub struct Router {
    routes: HashMap<&'static str, Handler>,
}

type Handler = fn(&Request) -> Response;

pub struct Request {
    pub path: String,
}

pub struct Response {
    pub status: u16,
}

pub fn mount() -> Router {
    let mut routes = HashMap::new();
    routes.insert("/health", health as Handler);
    routes.insert("/v1/events", ingest as Handler);
    Router { routes }
}

fn health(_req: &Request) -> Response {
    Response { status: 200 }
}

fn ingest(req: &Request) -> Response {
    tracing(&req.path);
    Response { status: 202 }
}

fn tracing(path: &str) {
    let _ = path;
}
EOF

cat > src/api/mod.rs <<'EOF'
pub mod routes;
EOF

cat > src/main.rs <<'EOF'
mod api;

fn main() {
    let router = api::routes::mount();
    let _ = router;
    println!("acme-app listening on :8080");
}
EOF

cat > Cargo.toml <<'EOF'
[package]
name = "acme-app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "acme-app"
path = "src/main.rs"
EOF

cat > README.md <<'EOF'
# acme-app

Demo service for the acme platform.
EOF

cat > .gitignore <<'EOF'
/target
EOF

git add -A
git -c user.name="Alex Arthurs" -c user.email="alex@acme.dev" commit -qm "Scaffold API routes"

# Bare origin at this point; the next commit puts us 1 ahead.
git clone -q --bare . "$ORIGIN"
git remote add origin "$ORIGIN"
git fetch -q origin
git branch -q --set-upstream-to=origin/main main

cat > src/lib.rs <<'EOF'
pub mod telemetry;
EOF

cat > src/telemetry.rs <<'EOF'
/// Record a named telemetry event.
pub fn record(event: &str) {
    let _ = event;
}
EOF

git add src/lib.rs src/telemetry.rs
git -c user.name="Alex Arthurs" -c user.email="alex@acme.dev" commit -qm "Add telemetry module" -- src/lib.rs src/telemetry.rs Cargo.toml

# Staged new file.
cat > docs/auth.md <<'EOF'
# Authentication

All API requests carry a bearer token in the `Authorization` header.

Tokens are minted by the auth service and expire after 24 hours.
EOF
git add docs/auth.md

# Unstaged modification.
cat >> src/api/routes.rs <<'EOF'
// TODO: rate limiting
EOF

# Child repo, dirty.
mkdir -p acme-sdk
cd acme-sdk
git init -q -b main
cat > sdk.ts <<'EOF'
export interface Event {
  name: string;
  payload: Record<string, unknown>;
}

export function send(event: Event): Promise<void> {
  return fetch("/v1/events", {
    method: "POST",
    body: JSON.stringify(event),
  }).then(() => undefined);
}
EOF
git add -A
git -c user.name="Alex Arthurs" -c user.email="alex@acme.dev" commit -qm "SDK scaffold"
cat >> sdk.ts <<'EOF'

export const VERSION = "0.2.0";
EOF

cd "$APP"
echo "--- parent status:"; git status --short --branch
echo "--- sdk status:"; git -C acme-sdk status --short
