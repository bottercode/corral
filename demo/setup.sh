# Sourced by demo.tape. Starts a corral server of its own (own HOME and
# TMPDIR, notifications off) with three stand-in agents and a shell, so
# recording never touches a real corral session.
root=/tmp/corral-demo
demo=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# Inside a corral pane these point at that pane's server; pin the socket to
# ours so nothing here can reach (or kill) it.
unset CORRAL_PANE CORRAL_BIN
export HOME=$root/home TMPDIR=$root/t/ CORRAL_SOCKET=$root/t/demo.sock
export PATH=$demo/bin:$demo/../target/release:/usr/bin:/bin:/usr/sbin:/sbin
export SHELL=/bin/bash PS1='\[\e[2m\]~/work\[\e[0m\] $ '

corral kill-server >/dev/null 2>&1
rm -rf "$root"
mkdir -p "$HOME/.config/corral" "$TMPDIR"
cat >"$HOME/.config/corral/config.toml" <<'EOF'
notifications = false
update_check = false
EOF
printf 'PS1=%q\n' "$PS1" >"$HOME/.bashrc"

repo() {
    mkdir -p "$root/work/$1"
    git -C "$root/work/$1" init -q -b "$2"
    git -C "$root/work/$1" -c user.name=demo -c user.email=demo@example.com \
        commit -q --allow-empty -m init
}
repo api rate-limits
repo auth fix-token-refresh
repo docs install-guide
repo web main

corral new --name api --cwd "$root/work/api" -- claude work "Add rate limiting" \
    "Read src/limits.rs" "Write TokenBucket" "Update middleware" "Run cargo check" >/dev/null
corral new --name fixer --cwd "$root/work/auth" -- claude block 9 \
    "Fix token refresh" "Read src/refresh.rs" "Add a refresh lock" >/dev/null
corral new --name docs --cwd "$root/work/docs" -- codex finish 6 \
    "Rewrite install guide" "Read README.md" "Edit docs/install.md" >/dev/null
corral new --name tests --cwd "$root/work/web" >/dev/null
cd "$root/work"
