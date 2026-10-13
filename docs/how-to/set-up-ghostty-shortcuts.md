# Set Up Ghostty Shortcuts for herdmux

This guide shows how to add macOS-native keyboard shortcuts in [Ghostty](https://ghostty.org) that control the herdmux sidebar without leaving the keyboard.

The herdmux tmux plugin registers a command table (`prefix o → s/t/e/1-9`) for manual use. It also registers direct prefix bindings (`C-s`, `C-t`, `M-1..9`) that terminal emulators can send programmatically. The `e` layout action is currently command-table only. The Ghostty config below uses the direct bindings so each shortcut is a single action.

## Prerequisites

- Ghostty 1.3.0 or newer (for key sequences and chained actions)
- herdmux tmux plugin installed and loaded
- tmux prefix set to `Ctrl-A` (`\x01`). If you use `Ctrl-B` (the default), replace `\x01` with `\x02` everywhere below.

## Shortcuts

| Ghostty shortcut | Action |
| --- | --- |
| `Cmd+E` | Toggle sidebar |
| `Cmd+Shift+O` | Focus sidebar |
| `Cmd+O` then `S` | Focus sidebar |
| `Cmd+O` then `T` | Toggle sidebar |
| `Cmd+O` then `1`–`9` | Switch to session by visible index |

## Configuration

Add this to your Ghostty config (`~/.config/ghostty/config`):

```
# Free Cmd+1..9 from Ghostty's default tab shortcuts so they can be
# used as the second half of the herdmux leader sequence.
keybind = cmd+1=unbind
keybind = cmd+2=unbind
keybind = cmd+3=unbind
keybind = cmd+4=unbind
keybind = cmd+5=unbind
keybind = cmd+6=unbind
keybind = cmd+7=unbind
keybind = cmd+8=unbind
keybind = cmd+9=unbind

# herdmux leader sequences: Cmd+O, then s/t/1..9
# These send prefix + direct bindings (C-s, C-t, M-1..9) to avoid
# timing issues with tmux command tables receiving all bytes at once.
keybind = cmd+o>s=text:\x01\x13
keybind = cmd+o>t=text:\x01\x14
keybind = cmd+o>1=text:\x01\x1b1
keybind = cmd+o>digit_1=text:\x01\x1b1
keybind = cmd+o>cmd+1=text:\x01\x1b1
keybind = cmd+o>cmd+digit_1=text:\x01\x1b1
keybind = cmd+o>2=text:\x01\x1b2
keybind = cmd+o>digit_2=text:\x01\x1b2
keybind = cmd+o>cmd+2=text:\x01\x1b2
keybind = cmd+o>cmd+digit_2=text:\x01\x1b2
keybind = cmd+o>3=text:\x01\x1b3
keybind = cmd+o>digit_3=text:\x01\x1b3
keybind = cmd+o>cmd+3=text:\x01\x1b3
keybind = cmd+o>cmd+digit_3=text:\x01\x1b3
keybind = cmd+o>4=text:\x01\x1b4
keybind = cmd+o>digit_4=text:\x01\x1b4
keybind = cmd+o>cmd+4=text:\x01\x1b4
keybind = cmd+o>cmd+digit_4=text:\x01\x1b4
keybind = cmd+o>5=text:\x01\x1b5
keybind = cmd+o>digit_5=text:\x01\x1b5
keybind = cmd+o>cmd+5=text:\x01\x1b5
keybind = cmd+o>cmd+digit_5=text:\x01\x1b5
keybind = cmd+o>6=text:\x01\x1b6
keybind = cmd+o>digit_6=text:\x01\x1b6
keybind = cmd+o>cmd+6=text:\x01\x1b6
keybind = cmd+o>cmd+digit_6=text:\x01\x1b6
keybind = cmd+o>7=text:\x01\x1b7
keybind = cmd+o>digit_7=text:\x01\x1b7
keybind = cmd+o>cmd+7=text:\x01\x1b7
keybind = cmd+o>cmd+digit_7=text:\x01\x1b7
keybind = cmd+o>8=text:\x01\x1b8
keybind = cmd+o>digit_8=text:\x01\x1b8
keybind = cmd+o>cmd+8=text:\x01\x1b8
keybind = cmd+o>cmd+digit_8=text:\x01\x1b8
keybind = cmd+o>9=text:\x01\x1b9
keybind = cmd+o>digit_9=text:\x01\x1b9
keybind = cmd+o>cmd+9=text:\x01\x1b9
keybind = cmd+o>cmd+digit_9=text:\x01\x1b9

# Direct shortcuts (skip the Cmd+O leader)
keybind = cmd+shift+o=text:\x01\x13
keybind = cmd+e=text:\x01\x14
```

## How it works

Ghostty's `text:` action sends raw bytes to the terminal. tmux command tables require keys to arrive as separate events, but `text:` sends everything in one write. To work around this, the config sends bytes that map to **direct prefix bindings** instead of going through the command table:

| Bytes sent | tmux sees | Binding |
| --- | --- | --- |
| `\x01\x13` | `prefix C-s` | Focus sidebar |
| `\x01\x14` | `prefix C-t` | Toggle sidebar |
| `\x01\x1b1` | `prefix M-1` | Switch to session 1 |

The `digit_1` and `cmd+1` variants handle different keyboard layouts and the case where `Cmd` is still held during the sequence.

## Why not send the command table keys directly?

When Ghostty sends `\x01ot` (prefix, `o`, `t`) as text, all three bytes arrive in a single `write()` call. tmux reads them in one chunk and the command table switch triggered by `o` does not complete before `t` is consumed. The direct bindings avoid this because `\x01\x14` is just prefix + one key — no table switch needed.

## Adapting for Ctrl-B prefix

If your tmux prefix is `Ctrl-B` (the default), replace every `\x01` with `\x02`:

```
keybind = cmd+e=text:\x02\x14
keybind = cmd+shift+o=text:\x02\x13
keybind = cmd+o>1=text:\x02\x1b1
# ... and so on
```
