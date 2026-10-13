# Get Started In tmux

This tutorial gets herdmux running as a real tmux sidebar, either through TPM or from a local clone. By the end, you will be able to press `prefix o → s` to open the sidebar, toggle it with `prefix o → t`, spread non-sidebar panes with `prefix o → e`, switch directly with `prefix o → 1` through `prefix o → 9`, and see agent and Git state update live.

## Prerequisites

- tmux installed
- Either TPM already installed or a local checkout of this repository
- `curl` or `wget` if you install with TPM, so herdmux can download prebuilt release binaries
- Rust/Cargo only if you are developing locally or your platform is not covered by the release binaries

## 1. Fast path with TPM

If you already use [TPM](https://github.com/tmux-plugins/tpm), run:

```bash
grep -q "AfreedHassan/herdmux" ~/.tmux.conf 2>/dev/null || printf '\nset -g @plugin '\''AfreedHassan/herdmux'\''\n' >> ~/.tmux.conf && tmux source-file ~/.tmux.conf && ~/.tmux/plugins/tpm/bin/install_plugins
```

Result: TPM clones the GitHub repo for you. The first time tmux loads the plugin, herdmux downloads the matching prebuilt binaries into `~/.tmux/plugins/herdmux/bin/`.

If you do not use TPM yet, continue with the local-clone flow below.

## 2. Local development only: build from source

Skip this step for normal TPM installs. If you run from a local clone or need to build for an unsupported platform, build the Rust binaries from the repository root:

```bash
cargo build --release
```

Result: the plugin can run binaries from `target/release/` instead of downloaded release binaries in `bin/`.

## 3. Add herdmux to your tmux config

Open `~/.tmux.conf` and add one of these minimal entries:

If you use TPM:

```tmux
set -g @plugin 'AfreedHassan/herdmux'
```

If you run from a local clone instead:

```tmux
source-file /absolute/path/to/herdmux/herdmux.tmux
```

If you added the TPM plugin line manually instead of using the one-line installer above, reload tmux and install plugins with:

```bash
tmux source-file ~/.tmux.conf
~/.tmux/plugins/tpm/bin/install_plugins
```

All `@herdmux-*` tmux options already have defaults. Configure width from the in-sidebar width slider or `~/.config/herdmux/config.json`; add tmux overrides only for global keys, for example:

```tmux
set -g @herdmux-focus-global-key "C-e"
```

Result: tmux registers direct prefix bindings for sidebar toggle, focus, and session switching.

## 4. Reload tmux configuration

Run:

```bash
tmux source-file ~/.tmux.conf
```

If you are using TPM and have not installed plugins yet, also run:

```bash
~/.tmux/plugins/tpm/bin/install_plugins
```

Result: the new keybinding is active in your current tmux server.

Recommended shortcut scheme:

- `prefix o → s` reveals and focuses the sidebar pane.
- `prefix o → t` toggles the sidebar.
- `prefix o → e` spreads non-sidebar panes in the current window using `even-horizontal`.
- `prefix o → 1` through `prefix o → 9` switch directly to the visible session indices.

If you use a terminal or window manager setup where no-prefix bindings are safe, you can also set `@herdmux-focus-global-key` and `@herdmux-index-keys`, but they are left unset by default to avoid conflicts.

## 5. Open the sidebar

Inside tmux, press:

```text
prefix o → s
```

Result: tmux reveals and focuses the sidebar. If the server is not running yet, the helper script starts it first.

## 6. Verify the sidebar is live

Use the sidebar to:

1. Move selection with `j` and `k` or the arrow keys.
2. Press `Enter` to switch sessions.
3. Press `n` or `c` to create a new tmux session.
4. Press `t` to open the theme picker.

Result: you should see the session list update and tmux switch the attached client to the selected session.

## 7. Verify agent detection

In any tmux session whose working directory matches a repo you use with a supported agent, start one of these tools:

- Amp
- Claude Code
- Codex
- OpenCode

Result: the session row should show a live status marker, and the detail panel should show thread-level information when available.

## Expected Outcome

You now have herdmux wired into tmux as a toggleable sidebar. From here you can move on to:

- [Configuration reference](../reference/configuration.md)
- [Features and keybindings reference](../reference/features-and-keybindings.md)
- [Architecture explanation](../explanation/architecture.md)
