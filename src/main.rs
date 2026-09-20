// lvim-zpm — a tpm-like plugin manager for zellij.
//
// tpm works because tmux gives a repo a hook (`run '…/tpm/tpm'` executes arbitrary shell at every
// tmux start) and dynamic config commands (`tmux bind-key …` reshapes the RUNNING server). zellij
// has neither — its config is declarative KDL read at session start — but it gives a PLUGIN the
// same two powers: `run_command` (host commands: git) and `reconfigure()` (merge a partial config —
// keybinds included — into the LIVE session). So the manager IS a zellij plugin, bootstrapped once
// into `load_plugins`, and on every session start it does what tpm does on every tmux start:
//
//   1. reads the user's manifest        ~/.config/zellij/zpm.kdl   (one entry per wanted plugin:
//      `"owner/repo"` cloned from GitHub, or `"file:/abs/dir"` used in place for local work)
//   2. ensures the clones exist         ~/.config/zellij/zpm/plugins/<name>/
//   3. reads each plugin's OWN manifest  <root>/zpm.kdl  — the convention this manager defines,
//      since a zellij plugin is inert wasm with no way to declare its keybinds (unlike a tmux
//      plugin, which ships arbitrary shell):
//
//        plugin {
//            wasm "zellij/plugin.wasm"                    // the built wasm, relative to the repo
//            permissions "ReadApplicationState" "…"        // what the wasm asks for on load
//            keybinds { … bind "…" { MessagePlugin "{{wasm}}" { …; }; } … }
//        }
//
//   4. pre-grants each plugin's permissions in the permission cache (so no prompt eats a
//      keypress) and applies its `keybinds` block LIVE via reconfigure() — `{{wasm}}` replaced by
//      the real `file:` URL. The plugin itself launches lazily: the first keybind pipe starts it
//      with the keybind's own identity, the one every later pipe matches.
//
// The config file on disk is never touched: like tpm's bindings, everything managed lives in the
// runtime and is re-applied at every session start from the manifest — remove an entry and it is
// simply gone from the next session. The manager reacts to CLI pipes for the tpm-style verbs:
//
//   zellij pipe -p file:…/lvim-zpm.wasm -n status     what is installed, what is applied
//   zellij pipe -p file:…/lvim-zpm.wasm -n install    clone anything missing, re-apply
//   zellij pipe -p file:…/lvim-zpm.wasm -n update     git pull every git-sourced plugin, re-apply
//
// **Live bindings belong to a client, and a client that attaches loses them.** zellij keeps the
// runtime config per client id, and on attach overwrites it with the config the client brought from
// disk. A client entering a session that has no one else in it is handed id 1 again — the id this
// instance already runs for — so no new instance loads and nothing here would notice. What does
// arrive is `SessionUpdate`, sent the moment a client is added, carrying the session's client count:
// a rise in it re-applies the blocks of the last run (measured 2026-09-19: a jump from lvim-zclaude
// into another session left `Ctrl s a` dead there).
//
// Operations are serialized (one at a time; a pipe during a run is queued) and host work is async
// (`RunCommandResult` events route by the `zpm_step` context tag). The CLI gets an immediate
// acknowledgement — zellij's server caps a waiting CLI pipe at one second, and a git clone will
// not fit in it — and the finished report lands in ~/.config/zellij/zpm/last-report and in the
// zellij log, prefixed `lvim-zpm:` (a background plugin has no pane; those are its face).

use std::collections::BTreeMap;
use zellij_tile::prelude::*;

const MANIFEST: &str = "$HOME/.config/zellij/zpm.kdl";
const STORE: &str = "$HOME/.config/zellij/zpm/plugins";
const PERMS: &str = "$HOME/.cache/zellij/permissions.kdl";

// ── manifest parsing (controlled formats, documented above — no KDL crate needed) ────────────────

/// The quoted strings of one line: `bind "Ctrl h" …` → ["Ctrl h", …]. No escapes — KDL strings in
/// these manifests never contain a quote.
fn quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('"') {
        let Some(len) = rest[start + 1..].find('"') else {
            break;
        };
        out.push(rest[start + 1..start + 1 + len].to_string());
        rest = &rest[start + 1 + len + 1..];
    }
    out
}

/// A line stripped of `//` comments (quote-aware: a `//` inside a string stays).
fn uncommented(line: &str) -> &str {
    let mut in_string = false;
    let bytes = line.as_bytes();
    for i in 0..bytes.len() {
        match bytes[i] {
            b'"' => in_string = !in_string,
            b'/' if !in_string && i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

/// The user manifest's plugin entries: every quoted string on a non-comment line.
fn parse_user_manifest(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    for line in text.lines() {
        for q in quoted(uncommented(line)) {
            if !q.is_empty() {
                entries.push(q);
            }
        }
    }
    entries
}

/// The `keybinds { … }` block of a plugin manifest, braces included — the NODE, found as the first
/// non-comment line whose first word is `keybinds` (a plain text search would land on the word in
/// a header comment), then extracted by quote-aware brace matching, so a `{{wasm}}` inside a
/// string cannot derail it.
fn keybinds_block(text: &str) -> Option<String> {
    let mut offset = 0usize;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        if uncommented(line).split_whitespace().next() == Some("keybinds") {
            start = Some(offset);
            break;
        }
        offset += line.len();
    }
    let at = start?;
    let open = at + text[at..].find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    for i in open..bytes.len() {
        match bytes[i] {
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(format!("keybinds {}", &text[open..=i]));
                }
            }
            _ => {}
        }
    }
    None
}

// ── the managed plugins ──────────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Managed {
    name: String,             // basename of the repo / local dir
    github: Option<String>,   // "owner/repo" for git sources, None for local ones
    root: String,             // absolute directory on the host (filled by the sync step)
    wasm: Option<String>,     // absolute path of the built wasm inside the repo
    link: Option<String>,     // install the wasm as <config>/plugins/<link> (layout plugins)
    installed: Option<String>, // the resolved install path when `link` is set
    permissions: Vec<String>,
    keybinds: Option<String>, // ready-to-apply block ({{wasm}} already substituted)
    note: String,             // one status line for the report
    check: Option<String>,    // check verb: "same" | "diff" | anything else = could not tell
}

impl Managed {
    /// Parse this plugin's own zpm.kdl (root is already known). With `link`, the wasm is INSTALLED
    /// to the fixed plugins directory — the place layouts reference — and everything else (grants,
    /// `{{wasm}}`) points there; without it the repo's own wasm is the target.
    fn read_manifest(&mut self, text: &str, home: &str) {
        for line in text.lines() {
            let line = uncommented(line);
            let word = line.split_whitespace().next().unwrap_or("");
            match word {
                "wasm" => {
                    if let Some(rel) = quoted(line).into_iter().next() {
                        self.wasm = Some(format!("{}/{}", self.root, rel));
                    }
                }
                "link" => self.link = quoted(line).into_iter().next(),
                "permissions" => self.permissions = quoted(line),
                _ => {}
            }
        }
        if let (Some(link), Some(_)) = (&self.link, &self.wasm) {
            self.installed = Some(format!("{home}/.config/zellij/plugins/{link}"));
        }
        if let (Some(block), Some(target)) = (keybinds_block(text), self.target()) {
            self.keybinds = Some(block.replace("{{wasm}}", &format!("file:{target}")));
        }
    }

    /// The wasm path everything points at: the installed copy when linked, the repo one otherwise.
    fn target(&self) -> Option<String> {
        self.installed.clone().or_else(|| self.wasm.clone())
    }
}

// ── the manager ──────────────────────────────────────────────────────────────────────────────────

/// What a run is doing. status only reports; install also clones what is missing and applies;
/// update also `git pull`s the git sources first. Session start behaves like install — that is the
/// tpm move: the manifest is REALIZED on every start.
#[derive(Clone, Copy, PartialEq)]
enum Verb {
    Status,
    Check,
    Install,
    Update,
}

#[derive(Default)]
struct State {
    home: String,
    permitted: bool,
    busy: bool,
    verb: Option<Verb>,
    queued: Option<Verb>,
    plugins: Vec<Managed>,
    report: Vec<String>,
    /// The keybinds blocks the last run applied — what an attaching client gets back.
    applied: Vec<String>,
    /// Clients connected to this session at the last `SessionUpdate`.
    clients: Option<usize>,
}

/// Whether a session update means a client has come in: the count rose. The first update only sets
/// the baseline — the run that follows the grant applies for the client that is there.
///
/// An update read back from another server's metadata can be stale and fake a rise; that costs one
/// reconfigure the server finds unchanged, and nothing more.
fn client_arrived(before: Option<usize>, now: usize) -> bool {
    matches!(before, Some(before) if now > before)
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::RunCommands,                  // git + reading manifests + the grant step
            PermissionType::Reconfigure,                  // apply keybinds to the live session
            PermissionType::ReadCliPipes,                 // answer the status/install/update verbs
            PermissionType::ReadApplicationState,         // SessionUpdate: a client attached
        ]);
        subscribe(&[
            EventType::RunCommandResult,
            EventType::PermissionRequestResult,
            EventType::SessionUpdate,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                if !self.permitted {
                    self.permitted = true;
                    self.begin(Verb::Install); // session start = realize the manifest
                }
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                eprintln!("lvim-zpm: permissions denied — the manager can do nothing");
            }
            Event::RunCommandResult(exit, stdout, stderr, context) => {
                let step = context.get("zpm_step").cloned().unwrap_or_default();
                let out = String::from_utf8_lossy(&stdout).to_string();
                let err = String::from_utf8_lossy(&stderr).to_string();
                self.on_step(&step, exit.unwrap_or(-1), &out, &err);
            }
            Event::SessionUpdate(sessions, _) => {
                let Some(here) = sessions.iter().find(|s| s.is_current_session) else {
                    return false;
                };
                let now = here.connected_clients;
                if !self.permitted {
                    // An instance loaded while nobody was attached (a reload of a detached
                    // session) never hears `Granted` — the answer is addressed to a client that is
                    // not there — and would refuse every pipe forever. This event is filtered by
                    // `ReadApplicationState`, one of the four asked for together: its arrival IS
                    // the grant. Measured 2026-09-19 on EXAMS after `start-or-reload-plugin`.
                    self.permitted = true;
                    self.clients = Some(now);
                    self.begin(Verb::Install);
                    return false;
                }
                if client_arrived(self.clients, now) {
                    if !self.applied.is_empty() {
                        // Straight from memory, not a new run: the person who just arrived is
                        // about to press a key, and a manifest read is a host round-trip.
                        for keybinds in &self.applied {
                            reconfigure(keybinds.clone(), false);
                        }
                        eprintln!(
                            "lvim-zpm: client {} attached — keybinds re-applied",
                            get_plugin_ids().client_id
                        );
                    } else if !self.busy {
                        self.begin(Verb::Install);
                    }
                }
                self.clients = Some(now);
            }
            _ => {}
        }
        false // nothing to render, ever
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        let verb = match message.name.as_str() {
            "status" => Verb::Status,
            "check" => Verb::Check,
            "install" => Verb::Install,
            "update" => Verb::Update,
            _ => return false, // not ours (e.g. another plugin's broadcast)
        };
        // Acknowledge the CLI at once — the server caps a waiting CLI pipe at one second, which no
        // git clone fits in — and point it at where the finished report lands.
        if let PipeSource::Cli(id) = &message.source {
            let state = if !self.permitted {
                "waiting for permissions"
            } else if self.busy {
                "queued behind the current run"
            } else {
                "running"
            };
            cli_pipe_output(
                id,
                &format!(
                    "lvim-zpm: {} {state} — report: ~/.config/zellij/zpm/last-report (and the zellij log)\n",
                    message.name
                ),
            );
            unblock_cli_pipe_input(id);
        }
        if !self.permitted {
            return false;
        }
        if self.busy {
            self.queued = Some(verb); // latest wish wins; one at a time
            return false;
        }
        self.begin(verb);
        false
    }
}

impl State {
    fn begin(&mut self, verb: Verb) {
        self.busy = true;
        self.verb = Some(verb);
        self.plugins.clear();
        self.report.clear();
        run_step(
            "manifest",
            &format!("mkdir -p \"{STORE}\"; echo \"$HOME\"; cat \"{MANIFEST}\" 2>/dev/null"),
        );
    }

    fn on_step(&mut self, step: &str, _exit: i32, out: &str, err: &str) {
        match step {
            "manifest" => self.on_manifest(out),
            "sync" => self.on_sync(out, err),
            "grant" => self.apply(),
            _ => {}
        }
    }

    /// The user manifest is read: build the plugin list and the sync script that ensures every
    /// clone exists (and pulls, on update), then prints each plugin's own manifest between
    /// markers — one host round-trip for everything.
    fn on_manifest(&mut self, out: &str) {
        let (home, manifest) = out.split_once('\n').unwrap_or(("", out));
        self.home = home.trim().to_string();
        let entries = parse_user_manifest(manifest);
        if entries.is_empty() {
            self.done(format!(
                "lvim-zpm: nothing to manage — list plugins in {} (\"owner/repo\" or \"file:/abs/dir\")",
                MANIFEST.replace("$HOME", "~")
            ));
            return;
        }
        let verb = self.verb.unwrap_or(Verb::Status);
        let mut script = String::new();
        for entry in entries {
            let (name, github, root) = if let Some(path) = entry.strip_prefix("file:") {
                let name = path.rsplit('/').next().unwrap_or(path).to_string();
                (name, None, path.to_string())
            } else {
                let name = entry.rsplit('/').next().unwrap_or(&entry).to_string();
                (name.clone(), Some(entry.clone()), format!("{STORE}/{name}"))
            };
            if let Some(repo) = &github {
                if verb == Verb::Install || verb == Verb::Update {
                    script.push_str(&format!(
                        "if [ ! -d \"{root}/.git\" ]; then \
                         git clone --depth 1 \"https://github.com/{repo}\" \"{root}\" >/dev/null 2>&1 \
                         || echo \"@@FAIL {name} clone failed\"; fi\n"
                    ));
                }
                if verb == Verb::Update {
                    script.push_str(&format!(
                        "git -C \"{root}\" pull --ff-only >/dev/null 2>&1 \
                         || echo \"@@FAIL {name} pull failed\"\n"
                    ));
                }
                if verb == Verb::Check {
                    // Fetch, then compare plain hashes — shallow-clone safe (no ancestry walk).
                    script.push_str(&format!(
                        "if [ -d \"{root}/.git\" ] && git -C \"{root}\" fetch -q origin >/dev/null 2>&1; then \
                         b=$(git -C \"{root}\" rev-parse --abbrev-ref HEAD 2>/dev/null); \
                         l=$(git -C \"{root}\" rev-parse HEAD 2>/dev/null); \
                         r=$(git -C \"{root}\" rev-parse \"origin/$b\" 2>/dev/null); \
                         if [ -n \"$l\" ] && [ \"$l\" = \"$r\" ]; then echo \"@@CHECK {name} same\"; \
                         else echo \"@@CHECK {name} diff\"; fi; \
                         else echo \"@@CHECK {name} unknown\"; fi\n"
                    ));
                }
            }
            script.push_str(&format!(
                "echo \"===PLUGIN {name}\"\ncat \"{root}/zpm.kdl\" 2>/dev/null || echo @@NOMANIFEST@@\n"
            ));
            self.plugins.push(Managed {
                name,
                github,
                root,
                ..Default::default()
            });
        }
        run_step("sync", &script);
    }

    /// Clones are in place and every plugin manifest is on stdout: parse them, then pre-grant the
    /// permissions (one more host round-trip) before anything gets loaded.
    fn on_sync(&mut self, out: &str, _err: &str) {
        for line in out.lines() {
            if let Some(fail) = line.strip_prefix("@@FAIL ") {
                self.report.push(format!("  ✗ {fail}"));
            }
            if let Some(check) = line.strip_prefix("@@CHECK ") {
                if let Some((name, state)) = check.split_once(' ') {
                    let name = name.to_string();
                    if let Some(plugin) = self.plugins.iter_mut().find(|p| p.name == name) {
                        plugin.check = Some(state.trim().to_string());
                    }
                }
            }
        }
        let mut sections = out.split("===PLUGIN ");
        sections.next(); // preamble (clone failures already collected)
        for section in sections {
            let (name, body) = section.split_once('\n').unwrap_or((section, ""));
            let name = name.trim().to_string();
            let Some(plugin) = self.plugins.iter_mut().find(|p| p.name == name) else {
                continue;
            };
            if body.contains("@@NOMANIFEST@@") {
                plugin.note = "✗ no zpm.kdl manifest".into();
                continue;
            }
            let home = self.home.clone();
            plugin.read_manifest(body, &home);
            plugin.note = match (&plugin.wasm, &plugin.keybinds, &plugin.link) {
                (Some(_), Some(_), _) => "✓".into(),
                (Some(_), None, Some(_)) => "✓ (layout plugin)".into(),
                (Some(_), None, None) => "✓ (no keybinds)".into(),
                (None, _, _) => "✗ manifest names no wasm".into(),
            };
        }
        if self.verb == Some(Verb::Status) || self.verb == Some(Verb::Check) {
            self.finish(); // status and check look (check also fetches), never touch
            return;
        }
        let mut script = String::from("mkdir -p \"$HOME/.cache/zellij\"\n");
        for plugin in &self.plugins {
            let Some(wasm) = &plugin.wasm else { continue };
            let Some(target) = plugin.target() else { continue };
            // A linked plugin's wasm is INSTALLED (copied when it changed) to the fixed plugins
            // directory, where layouts reference it by path.
            if let Some(installed) = &plugin.installed {
                script.push_str(&format!(
                    "mkdir -p \"$HOME/.config/zellij/plugins\"\n\
                     cmp -s \"{wasm}\" \"{installed}\" || cp \"{wasm}\" \"{installed}\"\n"
                ));
            }
            if plugin.permissions.is_empty() {
                continue;
            }
            let block = plugin
                .permissions
                .iter()
                .map(|p| format!("    {p}\\n"))
                .collect::<String>();
            // REWRITE the grant, never append-once: zellij sometimes rewrites this file from a
            // live server's memory and resurrects a stale block, which would then shadow the
            // manifest's truth forever (a save key failing with "denied" was exactly that). The
            // awk drops the existing block for this wasm, the printf appends the current one.
            script.push_str(&format!("t=\"{PERMS}.zpm.$$\"\n"));
            script.push_str(&format!(
                "awk -v k='\"{target}\" {{' 'BEGIN{{s=0}} index($0,k)==1{{s=1;next}} s{{if($0~/^}}/)s=0;next}} {{print}}' \"{PERMS}\" 2>/dev/null > \"$t\"\n"
            ));
            script.push_str(&format!(
                "printf '\"%s\" {{\\n{block}}}\\n' \"{target}\" >> \"$t\"\n"
            ));
            script.push_str(&format!("mv \"$t\" \"{PERMS}\"\n"));
        }
        run_step("grant", &script);
    }

    /// Permissions are cached: apply every keybinds block to the live session. This is the moment
    /// a session gains its managed bindings. The plugins themselves launch LAZILY — the first
    /// keybind pipe starts its target with the keybind's own identity, the only identity later
    /// pipes will match (a pre-warmed instance would carry a different one and idle forever).
    fn apply(&mut self) {
        self.applied.clear();
        for plugin in &self.plugins {
            if let Some(keybinds) = &plugin.keybinds {
                reconfigure(keybinds.clone(), false); // live only — the config file is not ours
                self.applied.push(keybinds.clone());
            }
        }
        self.finish();
    }

    fn finish(&mut self) {
        let verb = match self.verb {
            Some(Verb::Status) => "status",
            Some(Verb::Check) => "check",
            Some(Verb::Update) => "update",
            _ => "install",
        };
        // The client this instance answers for: zellij runs one instance per client, and a report
        // that does not say whose it is cannot tell a working session from a stale instance.
        let mut lines = vec![format!("lvim-zpm {verb} (client {}):", get_plugin_ids().client_id)];
        for plugin in &self.plugins {
            let source = plugin.github.as_deref().unwrap_or(&plugin.root);
            let applied = if self.verb == Some(Verb::Check) {
                match plugin.check.as_deref() {
                    Some("same") => " — up to date",
                    Some("diff") => " — UPDATE AVAILABLE (run update)",
                    Some(_) => " — could not check",
                    None => " — local, nothing to check",
                }
            } else if self.verb == Some(Verb::Status) {
                ""
            } else if plugin.keybinds.is_some() {
                " — keybinds applied"
            } else if plugin.installed.is_some() {
                " — wasm installed"
            } else {
                ""
            };
            lines.push(format!("  {} {} [{}]{}", plugin.note, plugin.name, source, applied));
        }
        lines.extend(self.report.iter().cloned());
        self.done(lines.join("\n"));
    }

    /// End the run: log the report, persist it where the CLI acknowledgement pointed
    /// (~/.config/zellij/zpm/last-report), start whatever was queued meanwhile.
    fn done(&mut self, report: String) {
        eprintln!("{report}");
        let quoted = report.replace('\'', "'\\''");
        run_step(
            "report",
            &format!("mkdir -p \"$HOME/.config/zellij/zpm\"; printf '%s\\n' '{quoted}' > \"$HOME/.config/zellij/zpm/last-report\""),
        );
        self.busy = false;
        self.verb = None;
        if let Some(verb) = self.queued.take() {
            self.begin(verb);
        }
    }
}

/// One async host step: `sh -c <script>`, its result routed back by the context tag.
fn run_step(step: &str, script: &str) {
    let mut context = BTreeMap::new();
    context.insert("zpm_step".to_string(), step.to_string());
    run_command(&["sh", "-c", script], context);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The host import every zellij-tile call goes through. A native test binary has no host, and
    // the linker wants the symbol all the same; nothing here ever calls it.
    #[unsafe(no_mangle)]
    extern "C" fn host_run_plugin_command() {}

    /// A client coming in raises the count; that and only that re-applies. The first update sets
    /// the baseline, and a client leaving is not a reason to touch anything.
    #[test]
    fn only_a_rise_in_clients_is_an_arrival() {
        assert!(!client_arrived(None, 1), "the first update is the baseline");
        assert!(client_arrived(Some(0), 1), "a detached session getting its client back");
        assert!(client_arrived(Some(1), 2), "a second terminal attaching");
        assert!(!client_arrived(Some(1), 1), "a pane moved, nobody came");
        assert!(!client_arrived(Some(2), 1), "a client left");
    }
}
