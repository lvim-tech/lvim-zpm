#!/usr/bin/env sh
#
# lvim-zpm — the one-time bootstrap (the `git clone tpm && add the run line` equivalent).
#
# Zellij has no hook that runs anything out of a repo, so the manager itself needs one manual
# install; from then on IT is the installer. This script — idempotent, safe to re-run:
#
#   1. copies the shipped manager  lvim-zpm.wasm  →  <config>/plugins/
#   2. adds it to `load_plugins` in config.kdl (ONE background instance per session; the config is
#      backed up first and the patched file is validated with `zellij setup --check` — a failure
#      restores it untouched)
#   3. pre-grants the manager's permissions, so its first run never waits on a prompt
#   4. seeds ~/.config/zellij/zpm.kdl with a commented example, when none exists
#
# Then declare plugins in zpm.kdl and either start a new session or run
# `zellij pipe -p file:<plugins>/lvim-zpm.wasm -n install` inside a running one.
#
set -eu

say() { printf 'lvim-zpm: %s\n' "$1"; }
fail() {
    printf 'lvim-zpm: %s\n' "$1" >&2
    exit 1
}

repo=$(cd "$(dirname "$0")" && pwd)
config_dir=${ZELLIJ_CONFIG_DIR:-"$HOME/.config/zellij"}
cache_dir=${ZELLIJ_CACHE_DIR:-"$HOME/.cache/zellij"}
config="$config_dir/config.kdl"
wasm_src="$repo/lvim-zpm.wasm"
wasm="$config_dir/plugins/lvim-zpm.wasm"
url="file:$wasm"

[ -f "$wasm_src" ] || fail "shipped manager not found: $wasm_src (build it: cargo build --release --target wasm32-wasip1)"
[ -f "$config" ] || fail "no Zellij config at $config — start Zellij once (it writes one), then re-run"

# ── 1. the manager ───────────────────────────────────────────────────────────
if [ -f "$wasm" ] && cmp -s "$wasm_src" "$wasm"; then
    say "manager already installed: $wasm"
else
    mkdir -p "$config_dir/plugins"
    cp "$wasm_src" "$wasm"
    say "manager installed: $wasm"
fi

# ── 2. load_plugins ──────────────────────────────────────────────────────────
if grep -q "^[[:space:]]*\"file:[^\"]*lvim-zpm\.wasm\"" "$config"; then
    say "config.kdl already loads the manager"
else
    backup="$config.lvim-zpm.bak"
    cp "$config" "$backup"
    patched="$config.lvim-zpm.tmp"
    awk -v url="$url" '
        !did && /^[[:space:]]*load_plugins[[:space:]]*{/ {
            print
            indent = $0; sub(/[^ \t].*$/, "", indent); indent = indent "    "
            printf "%s// lvim-zpm: the plugin manager — realizes ~/.config/zellij/zpm.kdl on every session start\n", indent
            printf "%s\"%s\"\n", indent, url
            did = 1
            next
        }
        { print }
        END {
            if (!did) {
                print ""
                print "// lvim-zpm: the plugin manager — realizes ~/.config/zellij/zpm.kdl on every session start"
                print "load_plugins {"
                printf "    \"%s\"\n", url
                print "}"
            }
        }
    ' "$config" > "$patched"
    mv "$patched" "$config"
    if command -v zellij >/dev/null 2>&1; then
        if ! ZELLIJ_CONFIG_FILE="$config" zellij setup --check >/dev/null 2>&1; then
            mv "$backup" "$config"
            fail "patched config failed \`zellij setup --check\` — original restored, nothing changed"
        fi
    fi
    say "manager added to load_plugins (backup: $backup)"
fi

# ── 3. permissions ───────────────────────────────────────────────────────────
perms="$cache_dir/permissions.kdl"
if [ -d "$cache_dir" ] && { [ ! -f "$perms" ] || ! grep -q "lvim-zpm\.wasm" "$perms"; }; then
    {
        printf '"%s" {\n' "$wasm"
        printf '    RunCommands\n'
        printf '    Reconfigure\n'
        printf '    ReadCliPipes\n'
        printf '}\n'
    } >> "$perms"
    say "permissions pre-granted: $perms"
else
    say "permissions already granted (or no cache dir yet — Zellij will ask once)"
fi

# ── 4. the manifest ──────────────────────────────────────────────────────────
manifest="$HOME/.config/zellij/zpm.kdl"
if [ -f "$manifest" ]; then
    say "manifest exists: $manifest"
else
    {
        printf '// lvim-zpm — the plugins this machine wants, one per line.\n'
        printf '// "owner/repo" is cloned from GitHub into ~/.config/zellij/zpm/plugins/;\n'
        printf '// "file:/abs/dir" is a local repo used in place. Each repo carries its own zpm.kdl.\n'
        printf 'plugins {\n'
        printf '    // "lvim-tech/lvim-winnav"\n'
        printf '}\n'
    } > "$manifest"
    say "manifest seeded: $manifest"
fi

say "done — new sessions realize the manifest; or: zellij pipe -p \"$url\" -n install"
