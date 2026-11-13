#!/usr/bin/env python3
"""PreToolUse hook for the Bash and PowerShell tools.

Blocks Claude attribution text:
1. A commit message with a "Co-Authored-By: Claude" trailer.
2. A pull request body with "Generated with/by Claude (Code)".

Exit 2 blocks the tool call. Claude Code shows stderr to the agent.
"""
import json
import re
import shlex
import sys
from pathlib import Path

COMMIT_CMD = re.compile(r"\bgit\b[^|;&]*\bcommit\b")
PR_CMD = re.compile(r"\bgh\s+(pr\s+(create|edit|merge)|api\b[^|;&]*\bpulls\b)")

CO_AUTHOR = re.compile(
    r"co-authored-by:[^\n]*(claude|anthropic\.com)", re.IGNORECASE
)
GENERATED = re.compile(
    r"generated\s+(with|by)\s*\[?\s*claude|claude\.com/claude-code", re.IGNORECASE
)

# Options whose value is a file that holds the message or body.
FILE_OPTS = {"-F", "--file", "--body-file", "-t", "--template"}


def referenced_files(command: str, cwd: str) -> str:
    try:
        tokens = shlex.split(command, posix=True)
    except ValueError:
        return ""
    text = []
    for i, tok in enumerate(tokens):
        value = None
        if tok in FILE_OPTS and i + 1 < len(tokens):
            value = tokens[i + 1]
        else:
            for opt in FILE_OPTS:
                if opt.startswith("--") and tok.startswith(opt + "="):
                    value = tok.split("=", 1)[1]
        if not value or value == "-":
            continue
        path = Path(value).expanduser()
        if not path.is_absolute():
            path = Path(cwd) / path
        try:
            text.append(path.read_text(errors="ignore"))
        except OSError:
            pass
    return "\n".join(text)


def main() -> int:
    try:
        data = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0
    if data.get("tool_name") not in ("Bash", "PowerShell"):
        return 0
    command = data.get("tool_input", {}).get("command", "")
    is_commit = bool(COMMIT_CMD.search(command))
    is_pr = bool(PR_CMD.search(command))
    if not (is_commit or is_pr):
        return 0

    text = command + "\n" + referenced_files(command, data.get("cwd", "."))
    if is_commit and CO_AUTHOR.search(text):
        print(
            "Blocked: the commit message has a 'Co-Authored-By: Claude' trailer. "
            "Remove the trailer and run the commit again.",
            file=sys.stderr,
        )
        return 2
    if is_pr and GENERATED.search(text):
        print(
            "Blocked: the pull request body has a 'Generated with Claude Code' line. "
            "Remove the line and run the command again.",
            file=sys.stderr,
        )
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
