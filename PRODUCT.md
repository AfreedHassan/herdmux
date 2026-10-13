# herdmux Product Context

## Register

Product. herdmux is a task-focused terminal sidebar for developers who already live in tmux.

## Purpose

herdmux keeps session switching, agent status, repo context, localhost links, and worktree navigation visible inside the user’s existing terminal workflow. It should make a crowded tmux server feel legible without replacing tmux.

## Target Users

- Developers running many tmux sessions, windows, worktrees, and AI agent processes at once.
- Terminal-first users who value fast keyboard navigation and predictable state over decorative UI.
- Plugin users who expect herdmux to stay lightweight, native, and mux-aware.

## Design Principles

1. **Density is an affordance**: the sidebar should fit useful context in narrow panes without feeling padded or wasteful.
2. **Local interaction state stays local**: focus, active row, and transient keyboard movement should feel immediate in each sidebar client.
3. **Shared state is only for shared truths**: server-propagated state should represent tmux-wide or sidebar-wide facts, not per-window cursor intent.
4. **Terminal-native clarity wins**: use stable glyphs, consistent columns, and semantic color. Avoid visual effects that make state harder to parse.
5. **Fast paths stay fast**: switching sessions and reacting to user input must outrank background convergence work.

## Anti-References

- Heavy dashboard chrome inside a narrow terminal pane.
- Flickery, server-owned focus that contradicts the current tmux client.
- Wide gutters that consume scarce sidebar width.
