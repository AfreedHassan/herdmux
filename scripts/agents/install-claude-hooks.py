"""Idempotently add OpenSessions agent-state hooks to ~/.claude/settings.json."""
import json, os, shutil, sys, time
path = os.path.expanduser("~/.claude/settings.json")
settings = json.load(open(path)) if os.path.exists(path) else {}
if os.path.exists(path):
    shutil.copy2(path, f"{path}.bak.opensessions.{int(time.time())}")
# Hooks call the script from ~/.local/bin; copy it there first (see README).
cmd = '"$HOME/.local/bin/opensessions-agent-state" '
wanted = [
    ("PreToolUse", "AskUserQuestion|ExitPlanMode", "needs-input"),
    ("PermissionRequest", "*", "needs-input"),
    ("PostToolUse", "*", "running"),
    ("UserPromptSubmit", None, "running"),
    ("Stop", None, "done"),
    ("SessionEnd", None, "off"),
]
hooks = settings.setdefault("hooks", {})
added = 0
for event, matcher, state in wanted:
    groups = hooks.setdefault(event, [])
    command = cmd + state
    if any(h.get("command") == command for g in groups for h in g.get("hooks", [])):
        continue
    group = {"hooks": [{"type": "command", "command": command, "timeout": 5}]}
    if matcher is not None:
        group = {"matcher": matcher, **group}
    groups.append(group)
    added += 1
tmp = path + ".tmp"
json.dump(settings, open(tmp, "w"), indent=2)
os.replace(tmp, path)
print(f"{path}: added {added} hook(s)")
