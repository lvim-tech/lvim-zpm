# lvim-zpm

A tpm-like plugin manager for [zellij](https://zellij.dev): declare the plugins you want in one
manifest, and every session start realizes it — clones, permissions, live keybinds.

## Why a manager plugin

tpm works because tmux gives a repo a hook (`run '…/tpm/tpm'` executes shell at every tmux start)
and dynamic config commands (`tmux bind-key` reshapes the running server). zellij has neither — its
config is declarative KDL read at session start — but it gives a **plugin** the same two powers:
`run_command` (host commands: git) and `reconfigure()` (merge a partial config, keybinds included,
into the **live** session). So the manager is itself a zellij plugin, bootstrapped once into
`load_plugins`. Like tpm, everything it manages lives in the runtime and is re-applied on every
session start: `config.kdl` is never touched, and an entry removed from the manifest is simply gone
from the next session.

## Install (once)

```sh
git clone https://github.com/lvim-tech/lvim-zpm
cd lvim-zpm && ./install.sh
```

Copies the shipped `lvim-zpm.wasm` into `~/.config/zellij/plugins/`, adds it to `load_plugins`
(config backed up + validated, restored on failure), pre-grants its permissions and seeds the
manifest. Building from source (the shipped wasm makes this optional):
`cargo build --release --target wasm32-wasip1`.

## Declare plugins

`~/.config/zellij/zpm.kdl`:

```kdl
plugins {
    "lvim-tech/lvim-winnav"            // cloned from GitHub into ~/.config/zellij/zpm/plugins/
    // "file:/abs/path/to/a/local/repo"  // a local repo, used in place (development)
}
```

## The verbs

New sessions realize the manifest by themselves (that is the tpm move). Inside a running session:

```sh
zellij pipe -p file:~/.config/zellij/plugins/lvim-zpm.wasm -n status    # what is installed/applied
zellij pipe -p file:~/.config/zellij/plugins/lvim-zpm.wasm -n check     # fetch + report who has updates (touches nothing)
zellij pipe -p file:~/.config/zellij/plugins/lvim-zpm.wasm -n install   # clone missing, re-apply
zellij pipe -p file:~/.config/zellij/plugins/lvim-zpm.wasm -n update    # git pull all, re-apply
```

The CLI answers at once (zellij caps a waiting CLI pipe at one second — no git clone fits in it);
the finished report lands in `~/.config/zellij/zpm/last-report` and in the zellij log (`lvim-zpm:`
prefix).

## What a managed plugin ships: `zpm.kdl`

A zellij plugin is inert wasm — it cannot declare its own keybinds the way a tmux plugin ships
shell. lvim-zpm defines that convention: a `zpm.kdl` at the plugin repo's root.

```kdl
plugin {
    wasm "zellij/my-plugin.wasm"                       // the BUILT wasm, relative to the repo
    permissions "ReadApplicationState" "WriteToStdin"   // pre-granted, so no prompt eats a keypress
    keybinds {
        normal {
            bind "Ctrl h" { MessagePlugin "{{wasm}}" { name "move"; payload "left"; }; }
        }
    }
}
```

A **layout** plugin (a status bar, anything living in a layout pane) has no keybinds; instead it
declares `link` and the manager keeps a copy of its wasm INSTALLED at the fixed path layouts
reference (`~/.config/zellij/plugins/<link>`), refreshed whenever the repo's wasm changes:

```kdl
plugin {
    wasm "my-bar.wasm"
    link "my-bar.wasm"          // installed as ~/.config/zellij/plugins/my-bar.wasm
    permissions "ReadApplicationState"
}
```

With `link`, permissions and `{{wasm}}` point at the installed copy.

`{{wasm}}` is replaced with the absolute `file:` URL of the built wasm. The `keybinds` block is
ordinary zellij config syntax, applied live via `reconfigure()`. Repos are expected to **ship the
built wasm** — the manager clones and applies; it does not build.

**Keybind plugins also want a `load_plugins` entry** in `config.kdl`, with the same URL the
keybinds pipe to (`file:~/.config/zellij/zpm/plugins/<name>/<wasm>`): a preloaded background
instance is what a keybind pipe reaches. Without it the first keypress launches the target — into
a visible tiled pane, which is how zellij launches a keybind pipe's missing target. (This cannot
be done live: `load_plugins` is only read at session start, and reconfigure() merging it is a
no-op.)

**A client that enters the session gets the keybinds back.** zellij keeps the runtime config per
client id and replaces an attaching client's with the one it carries from disk — which has none of
this. The manager watches the session's client count and re-applies the last run's blocks the
moment it rises, so jumping in from another session, switching with a session manager or a plain
`zellij attach` all keep the managed keys.

## Notes

- One manager instance runs **per client**, not per session: zellij loads a plugin again for every
  client id that enters. Each instance applies for its own client, and its reports say which
  (`lvim-zpm install (client 2):`).
- `start-or-reload-plugin` reaches only the instances of clients that are attached right now; the
  rest keep the old wasm until the session is restarted.
- Operations are serialized; a verb piped during a run is queued.
- `status` only looks; it never clones, pulls or applies.
- The permission cache (`~/.cache/zellij/permissions.kdl`) is rewritten per plugin — the manager's
  own block too, by `install.sh` — because zellij may rewrite that file from a live server's memory
  and resurrect a stale block.
