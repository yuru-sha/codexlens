#!/usr/bin/env python3
"""Probe Codex sandbox denial using only synthetic files and loopback traffic."""

from pathlib import Path
import socket
import subprocess
import sys
import tempfile


PROBE = """
import errno, pathlib, socket, sys
try:
    if sys.argv[1] == 'write':
        pathlib.Path(sys.argv[2]).write_text('synthetic sandbox probe')
    else:
        with socket.create_connection(('127.0.0.1', int(sys.argv[2])), timeout=2):
            pass
except OSError as error:
    sys.exit(77 if error.errno in (errno.EPERM, errno.EACCES) else 78)
"""


def main():
    root = Path(__file__).resolve().parents[1]
    scratch = root / ".codexlens"
    scratch.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(dir=scratch) as directory, socket.socket() as server:
        base = Path(directory).resolve()
        workspace = base / "workspace"
        workspace.mkdir()
        server.bind(("127.0.0.1", 0))
        server.listen(4)
        port = str(server.getsockname()[1])
        command = [sys.executable, "-B", "-c", PROBE]
        control = subprocess.run(command + ["connect", port], capture_output=True, timeout=10)
        if control.returncode:
            raise RuntimeError("loopback control unavailable")
        cases = [
            ("workspace-write", "write", str(workspace / "allowed.txt"), 0),
            ("workspace-write", "write", str(base / "denied.txt"), 77),
            ("read-only", "write", str(workspace / "readonly.txt"), 77),
            ("workspace-write", "connect", port, 77),
            ("read-only", "connect", port, 77),
        ]
        for mode, action, target, expected in cases:
            result = subprocess.run([
                "codex", "sandbox",
                "-c", 'sandbox_mode="' + mode + '"',
                "-c", "sandbox_workspace_write.network_access=false",
                "-c", "sandbox_workspace_write.writable_roots=[]",
                "--", *command, action, target,
            ], cwd=workspace, capture_output=True, timeout=30)
            if result.returncode != expected:
                raise RuntimeError("{} {}: expected {}, got {}".format(
                    mode, action, expected, result.returncode))
            print("sandbox: PASS {} {} {}".format(
                mode, action, "allowed" if expected == 0 else "denied"))
    print("Explicit sandbox probes passed; verify the actual agent session separately.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        detail = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        print("sandbox: FAIL ({})".format(detail), file=sys.stderr)
        sys.exit(1)
