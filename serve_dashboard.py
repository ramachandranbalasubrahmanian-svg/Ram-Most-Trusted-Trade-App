#!/usr/bin/env python3
"""Launcher for the com.ramistp.dashboard KeepAlive service.

Run by the Full-Disk-Access-granted Framework python3 (the same interpreter the
scheduler uses) so it can reach the project under ~/Documents, which a bare
launchd /bin/bash cannot (TCC "Operation not permitted"). It then runs
`ram_istp serve <tf>` as a child from the project root and stays alive as the
supervised process, so launchd's KeepAlive watches it and respawns on exit
(a crash, or the scheduler's `launchctl kickstart -k`).
"""
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.abspath(__file__))
tf = sys.argv[1] if len(sys.argv) > 1 else "30min"

binary = os.path.join(ROOT, "target", "release", "ram_istp")
if not os.path.exists(binary):
    binary = os.path.join(ROOT, "target", "debug", "ram_istp")

sys.exit(subprocess.run([binary, "serve", tf], cwd=ROOT).returncode)
