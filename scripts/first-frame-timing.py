#!/usr/bin/env python3
"""PERF-T11 first-frame lane driver: interleaved A/B binaries, one PTY per sample.

Spawns each binary in a fresh 100x32 PTY with the extension-free workload
(mirrors scripts/verification/performance.ts runFirstFrameSample: same argv,
environment shape, and terminal size), measures wall time from exec to the
first complete DEC synchronized-output transaction (row-local fallback
recorded), then exits the child through /quit.

Usage:
  first-frame-timing.py --bin-a BASE --bin-b DESIGN [--pairs 9] [--warmup 1]

Sample order alternates per pair (A,B then B,A) to cancel drift; the per-run
medians of each arm are reported with the A/B win ratio. Run under taskset
externally (e.g. `taskset -c 20-40`) — affinity is inherited by children.
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import select
import struct
import subprocess
import statistics
import sys
import tempfile
import termios
import time
from pathlib import Path

EXTENSION_FREE_ARGS = [
    "--provider", "anthropic",
    "--model", "claude-sonnet-4-5",
    "--api-key", "verification-no-network",
    "--no-extensions",
    "--no-session",
    "--offline",
    "--no-context-files",
    "--no-skills",
    "--no-prompt-templates",
    "--no-themes",
    "--approve",
]

SYNC_BEGIN = b"\x1b[?2026h"
SYNC_END = b"\x1b[?2026l"
FRAME_DEADLINE_S = 20.0
EXIT_DEADLINE_S = 10.0


class FrameTimeout(RuntimeError):
    """Raised when no first frame is observed within the deadline."""


class ExitFailure(RuntimeError):
    """Raised when the child does not exit cleanly through /quit."""


def build_env(sandbox: str) -> dict[str, str]:
    home = os.path.join(sandbox, "home")
    agent_dir = os.path.join(sandbox, "agent")
    session_dir = os.path.join(sandbox, "sessions")
    for path in (home, agent_dir, session_dir):
        os.makedirs(path, exist_ok=True)
    return {
        "HOME": home,
        "PI_CODING_AGENT_DIR": agent_dir,
        "PI_CODING_AGENT_SESSION_DIR": session_dir,
        "PI_OFFLINE": "1",
        "PI_SKIP_VERSION_CHECK": "1",
        "TERM": "xterm-256color",
        "TERM_PROGRAM": "WarpTerminal",
        "COLORTERM": "truecolor",
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
    }


def strip_sequences(raw: bytes) -> bytes:
    out = bytearray()
    index = 0
    while index < len(raw):
        byte = raw[index]
        if byte == 0x1B:
            index += 1
            if index < len(raw) and raw[index:index + 1] == b"[":
                index += 1
                while index < len(raw) and not (0x40 <= raw[index] <= 0x7E):
                    index += 1
                index += 1
            elif index < len(raw) and raw[index:index + 1] == b"]":
                index += 1
                while index < len(raw) and raw[index] not in (0x07,):
                    if raw[index:index + 2] == b"\x1b\\":
                        index += 1
                        break
                    index += 1
                index += 1
            else:
                index += 1
            continue
        out.append(byte)
        index += 1
    return bytes(out)


def first_frame_elapsed(binary: str) -> tuple[float, str]:
    sandbox = tempfile.mkdtemp(prefix="frame-timing-")
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))
    proc = subprocess.Popen(
        [binary, *EXTENSION_FREE_ARGS],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        cwd=sandbox,
        env=build_env(sandbox),
        close_fds=True,
    )
    os.close(slave)
    start = time.perf_counter()
    try:
        buffer = b""
        seen_csi = False
        detection = ""
        deadline = start + FRAME_DEADLINE_S
        while time.perf_counter() < deadline:
            readable, _, _ = select.select([master], [], [], 0.25)
            if not readable:
                if proc.poll() is not None:
                    raise ExitFailure(
                        f"{binary} exited {proc.returncode} before first frame; "
                        f"tail={buffer[-400:]!r}"
                    )
                continue
            chunk = os.read(master, 65536)
            if not chunk:
                raise ExitFailure(
                    f"{binary} closed the pty before first frame; tail={buffer[-400:]!r}"
                )
            buffer += chunk
            if b"\x1b[" in buffer:
                seen_csi = True
            begin = buffer.find(SYNC_BEGIN)
            if begin >= 0 and buffer.find(SYNC_END, begin + len(SYNC_BEGIN)) >= 0:
                detection = "synchronized-output"
                break
            if seen_csi and len(strip_sequences(buffer).strip()) > 0:
                detection = "row-local-fallback"
                break
        else:
            raise FrameTimeout(
                f"{binary}: no first frame within {FRAME_DEADLINE_S}s; tail={buffer[-400:]!r}"
            )
        elapsed_ms = (time.perf_counter() - start) * 1000.0
        proc.stdin = None  # child stdin/stdout are the pty; do not double-close
        proc.terminate()
        try:
            proc.wait(timeout=0.5)
        except subprocess.TimeoutExpired:
            pass
        return elapsed_ms, detection
    finally:
        if proc.poll() is None:
            proc.kill()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
        os.close(master)


def median(values: list[float]) -> float:
    return statistics.median(values)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-a", required=True, help="baseline binary (before)")
    parser.add_argument("--bin-b", required=True, help="design binary (after)")
    parser.add_argument("--pairs", type=int, default=9)
    parser.add_argument("--warmup", type=int, default=1, help="warmup runs per arm")
    parser.add_argument("--json", action="store_true", help="machine-readable output")
    args = parser.parse_args()

    bin_a = Path(args.bin_a).resolve()
    bin_b = Path(args.bin_b).resolve()
    for label, path in (("A", bin_a), ("B", bin_b)):
        if not path.is_file():
            raise SystemExit(f"first-frame-timing: --bin-{label} not a file: {path}")

    for _ in range(args.warmup):
        first_frame_elapsed(str(bin_a))
        first_frame_elapsed(str(bin_b))

    samples: dict[str, list[tuple[float, str]]] = {"A": [], "B": []}
    for pair in range(args.pairs):
        order = ("A", "B") if pair % 2 == 0 else ("B", "A")
        for arm in order:
            path = bin_a if arm == "A" else bin_b
            elapsed, detection = first_frame_elapsed(path)
            samples[arm].append((elapsed, detection))
            print(
                f"pair {pair + 1}/{args.pairs} arm {arm}: {elapsed:.1f} ms ({detection})",
                flush=True,
            )

    times_a = [t for t, _ in samples["A"]]
    times_b = [t for t, _ in samples["B"]]
    median_a = median(times_a)
    median_b = median(times_b)
    win = median_a / median_b if median_b else float("inf")
    report = {
        "binA": str(bin_a),
        "binB": str(bin_b),
        "pairs": args.pairs,
        "aMs": sorted(round(t, 3) for t in times_a),
        "bMs": sorted(round(t, 3) for t in times_b),
        "aMedianMs": round(median_a, 3),
        "bMedianMs": round(median_b, 3),
        "win": round(win, 3),
        "aDetections": [d for _, d in samples["A"]],
        "bDetections": [d for _, d in samples["B"]],
    }
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(
            f"A (before): median {median_a:.1f} ms  range "
            f"{min(times_a):.1f}-{max(times_a):.1f}\n"
            f"B (after):  median {median_b:.1f} ms  range "
            f"{min(times_b):.1f}-{max(times_b):.1f}\n"
            f"win: {win:.2f}x"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
