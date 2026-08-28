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

## Notes

- One manager instance runs per session (a `load_plugins` background plugin). Operations are
  serialized; a verb piped during a run is queued.
- `status` only looks; it never clones, pulls or applies.
- The permission cache (`~/.cache/zellij/permissions.kdl`) is appended idempotently; zellij may
  rewrite that file, and the manager re-adds grants on the next run.
