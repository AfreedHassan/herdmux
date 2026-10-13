# Sidebar alignment sketch

Current live sidebar shape is roughly this — and it shows the alignment problem:

```text
   Sessions ◓ warming up…

    1 lazydiff
      main         +22 -13
    2 pi-config
      main           +7 -2
    3 CLIProxyAPI
      main  ⌁8317
 ▾ plane-ee-wt 2wt
     4 feat-databases
       feat/databases
     5 preview
       preview
 ▌  6 herdmux
      f…  ⌁41916 +779 -590
    7 effect-ts

    8 os-demo-agent-panel
      master        +12 -1
 ▾ os-demo-worktrees 2wt
     9 os-demo-feat-agent-
       feat/agent-p… +4 -0
    10 os-demo-preview
       preview       +5 -1
   11 os-demo-main
      main           +2 -0

 ─────────────────────────
 agents 2        a:current
 …ments/work/herdmux
 local localhost:41916

  ⠸ better grouping
    working · amp

  ⠸ ⠭ New grouping
    working · amp

 ─────────────────────────
 ⇥ cycle  ⏎ go  → agents
f filter  d hide  x kill
```

What’s wrong in that diagram:

```text
session rows:
    1 name
      metadata
    2 name
      metadata

worktree header:
 ▾ group-name 2wt       <-- starts too far left vs session text

worktree children:
     4 child
       metadata         <-- one extra indent level, but feels uneven

detail/footer:
 agents...
 …path                 <-- starts too far left / different grid
 ⇥ cycle...
f filter...            <-- wrapped footer loses left padding
```

Painter-inspired target grid:

```text
  Sessions                         11

│   01  inbox
│       main
│
│   02  docs-site
│       main
│
│   03  infra
│       infra/terraform
│
│   ▾ plane-ee-wt        2wt
│   │ 04  feat-databases
│   │     feat/databases
│   │
│   │ 05  preview
│   │     preview
│
│▌  06  herdmux
│       feat/fix-ratatui…    ⌁41916  +779  -590
│
│   07  api
│       api/main
│
│   08  cli
│       main
│
│   09  mobile
│       mobile/main
│
│   ▾ research           1wt
│   │ 10  spike-ui
│   │     spike/ui
│
│   11  experiments
│       exp/try-new-thing

──────────────────────────────────
  agents 2                 a:current
  ~/projects/herdmux
  local  localhost

  ⠸  a  ratatui-refactor       current
        thinking · 2m 14s

  ✓  b  keybindings
        idle · 47s

──────────────────────────────────
  ⇥ cycle          ⏎ go        → agents
  f filter         d hide      x kill
```

Main corrections from the generated mockup:

- Keep one stable visual grid across sessions, worktrees, details, and footer.
- Reserve a far-left rail/active-marker column. Inactive rows keep the space; only the current row shows `▌`.
- Use zero-padded session numbers (`01`, `02`, …) so the number column never jitters.
- Align names after the number column; align metadata under names.
- Worktree groups get a disclosure row, and children get a subtle nested rail/indent — enough hierarchy without making the whole list jagged.
- Detail panel starts at the same content inset as the session list, not at column 0.
- Agent rows should show useful session/thread information once, then status/time on the secondary line. Avoid repeating `amp · amp`-style metadata.
- Footer wraps on the same grid; the second footer line must not start at column 0.
