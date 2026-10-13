# herdmux Context

herdmux coordinates terminal multiplexer sessions, sidebar clients, and agent activity inside a local terminal workspace. This glossary names the product concepts that must stay stable across the server, sidebar UI, mux integration, and documentation.

## Language

**Server Generation**:
A single running herdmux server instance for one tmux socket, together with the sidebar clients connected to it. A Server Generation can be running, closing, or closed; once closing starts, the same generation must not become ready again.
_Avoid_: server process when discussing lifecycle ownership, client group, sidebar generation

**Quit**:
A request from a connected sidebar client to terminate the current Server Generation. Quit is global to that Server Generation, not a local close of one sidebar pane.
_Avoid_: close, hide, dismiss

**Lifecycle Operation**:
A server-owned message that changes shared sidebar lifecycle state for a Server Generation. Sidebar clients, mux hooks, and timers request lifecycle changes by sending Lifecycle Operations; they do not directly own the shared lifecycle state.
_Avoid_: direct coordinator mutation, ad hoc lifecycle flag, local lifecycle patch

**Lifecycle Channel**:
The serialized path through which Lifecycle Operations enter the server-owned state machine. A Lifecycle Channel rejects re-entrant lifecycle submissions while it is delivering effects from the current operation.
_Avoid_: callback-driven lifecycle mutation, nested lifecycle send

**Fixed Sidebar Width**:
The server-owned sidebar width for one Server Generation. It starts from persisted configuration and may be changed only through an explicit sidebar width command, such as the debounced live TUI width slider. Runtime width changes are saved back to configuration for the next server start. Observed tmux pane widths are drift signals only; they never author a new width. Every herdmux sidebar pane should be repaired back to the Fixed Sidebar Width.
_Avoid_: resize transaction, user-authored width, global width adoption

**Sidebar Presence Reconciliation**:
A Lifecycle Operation transaction that ensures every eligible tmux window in a Server Generation has exactly one herdmux sidebar. Eligible windows are unique non-stash tmux windows, deduplicated by tmux `window_id` so linked sessions do not create duplicate sidebars; targets are spawned one at a time in a staggered order that starts with the origin session.
_Avoid_: warmup when referring to the operation, spawn pass, ensure pass

**Presence Failure**:
A diagnosed Sidebar Presence Reconciliation target that did not connect because spawning failed, the window vanished, or the sidebar timed out before identifying. Presence Failures allow warmup to finish with evidence instead of staying stuck or silently pretending the target succeeded.
_Avoid_: missing sidebar when the failure is classified, ignored target

**Shared Sidebar State**:
The sidebar state that is common to every sidebar client in a Server Generation, such as the session list, agents, sidebar lifecycle, sidebar width, theme, filter, and collapsed groups. Shared Sidebar State must not contain a universal current session.
_Avoid_: server state when it includes per-client facts, global current session

**Client View State**:
The sidebar state that belongs to one connected terminal/tmux client, such as its current tmux session and selected sidebar row. Client View State is sent only to the relevant client and must not move other clients' current session or selection.
_Avoid_: global focus, shared current session, synchronized cursor

**Pending Switch Request**:
A local client command in flight that asks tmux to switch this terminal client to another session. A Pending Switch Request may drive transient cursor/navigation behavior, but it is not the current session and must not color the destination row as active until pane identity or a `your-session` acknowledgement confirms the switch.
_Avoid_: optimistic current session, intended active session, fake active row

**Agent Thread State**:
Canonical agent activity shown in the sidebar, sourced from agent-native thread data or explicit agent events with stable identity. Agent Thread State should not be inferred from tmux pane titles or commands.
_Avoid_: pane-derived agent row, synthetic agent row

**Agent Pane Presence**:
A tmux pane observation that an agent-looking process may exist. Agent Pane Presence is not a canonical sidebar row source because it lacks stable thread identity and can duplicate Agent Thread State.
_Avoid_: using pane title as agent identity, broad pane scan as agent source
