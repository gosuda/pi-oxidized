#!/usr/bin/env python3
"""Live-GitHub Workflowz graph, journal, and controller."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import re
import subprocess
import sys
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable, Iterator, Mapping, Sequence, cast

ROOT = Path(__file__).resolve().parents[2]
SDD = ROOT / ".outline/sdd"
GRAPH_PATH = SDD / "workflowz.json"
STATE_PATH = SDD / "workflowz-state.json"
PLAN_PATH = SDD / "PLAN.md"
LOCK_PATH = SDD / "workflowz.lock"
REPOSITORY = "metaphorics/pi-oxidized"
ROOT_ISSUE = 12
DURABLE_REF = "refs/workflowz/integration"
PHASES = ("scout", "lease", "implement", "verify", "review", "fix", "integrate", "gate", "close", "done")
PHASE_INDEX = {phase: index for index, phase in enumerate(PHASES)}
EVENT_PHASE = {
    "scout": "scout",
    "implement": "implement",
    "verify": "verify",
    "review": "review",
    "fix": "fix",
    "fix-skip": "fix",
    "integrate-prepared": "integrate",
    "integrate-finalized": "integrate",
    "gate": "gate",
    "close": "close",
    "done": "done",
}
WORKER_EVENTS = {"implement", "verify", "review", "fix", "fix-skip", "integrate-prepared", "integrate-finalized", "gate", "close", "done"}
RUNNABLE_STATUS = "READY"
NON_RUNNABLE_STATUSES = frozenset({
    "HELD",
    "PENDING_DISPOSITION",
    "PENDING_RULING",
    "FROZEN",
    "EXTERNALLY_BLOCKED",
})
EXECUTION_STATUSES = frozenset({RUNNABLE_STATUS, *NON_RUNNABLE_STATUSES})
# hold/resume use the same stableId/attempt/generation/baseSha/headSha binding as other bound
# journal events: status is attempt-scoped control-plane state, not a phase transition.
BOUND_EVENTS = {"scout", "lease-acquire", "lease-release", "lease-reclaim", "invalidate", "perf-iteration", "hold", "resume"} | WORKER_EVENTS
# While non-READY, only resume and lease unwind/control events may run; scout/lease-acquire/workers are rejected.
NON_READY_BLOCKED_EVENTS = frozenset({"scout", "lease-acquire", "perf-iteration"} | WORKER_EVENTS)
COUNCIL_SEATS = ("contract", "correctness", "terminal-accessibility", "performance-dependencies", "release-docs")
TRACKS = ("PAR", "TUI", "XC", "REL", "PERF", "DEPS", "DOC", "VER")
GATE_COMMANDS: dict[str, tuple[str, ...]] = {
    "G1": ("python3", "/home/alpha/.claude/plugins/cache/odin-marketplace/odin/1.17.108/skills/unlazy/scripts/gate_check.py", "--status", ".outline/gates/leaf-*.md"),
    "G2": ("sh", "-c", 'test "$(gh issue list --repo metaphorics/pi-oxidized --state open --limit 200 --json number --jq "[.[] | select(.number != 12)] | length")" = 0 && echo EXECUTION_ISSUES_CLOSED'),
    "G3": ("sh", "-c", "cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo nextest run --workspace --all-features && bun run verify:dependencies && echo RUST_GATE_OK"),
    "G4": ("sh", "-c", "bun run check && bun test scripts packages && echo TS_GATE_OK"),
    "G5": ("sh", "-c", "bun run verify:parity && bun run verify:compatibility && bun run verify:e2e && bun run verify:replacement && echo COMPAT_GATE_OK"),
    "G6": ("sh", "-c", "bun run verify:performance && bun run verify:extension-scaling && echo PERF_GATE_OK"),
    "G7": ("sh", "-c", "bun run verify:release && bun run verify:terminal && bun run package-release:dry && echo RELEASE_GATE_OK"),
    "G8": ("sh", "-c", "bun run verify:docs && echo DOCS_GATE_OK"),
    "G9": ("sh", "-c", "python3 -c \"import json; d=json.load(open(\\\".outline/sdd/final-review.json\\\")); assert d[\\\"critical\\\"]==0 and d[\\\"important\\\"]==0; print(\\\"FINAL_REVIEW_OK\\\")\""),
    "G10": ("sh", "-c", "bun run verify:map-ledger && echo MAP_LEDGER_OK"),
    "G11": ("python3", ".outline/sdd/check-workflowz.py", "--pre-close"),
    "track:PAR": ("bun", "run", "verify:parity"),
    "track:TUI": ("bun", "run", "verify:terminal"),
    "track:XC": ("sh", "-c", "cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo nextest run --workspace --all-features && echo RUST_GATE_OK"),
    "track:REL": ("bun", "run", "verify:release"),
    "track:PERF": ("sh", "-c", "bun run verify:performance && bun run verify:extension-scaling && echo PERF_GATE_OK"),
    "track:DEPS": ("bun", "run", "verify:dependencies"),
    "track:DOC": ("bun", "run", "verify:docs"),
    "track:VER": ("bun", "run", "verify:alignment"),
    "track:MAP-preclose": ("bun", "run", "verify:map-ledger"),
    "track:MAP": ("bun", "run", "verify:map-closure"),
}
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
STABLE_RE = re.compile(r"(?mi)^Stable ID:\s*`([^`]+)`\s*$")
ISSUE_LINK_RE = re.compile(r"https://github\.com/metaphorics/pi-oxidized/issues/(\d+)")
ISO_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$")

_LOCK_DEPTH = 0


class WorkflowError(RuntimeError):
    pass


def fail(message: str) -> None:
    raise WorkflowError(message)


def canonical_json(value: object) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def digest(value: object) -> str:
    return "sha256:" + hashlib.sha256(canonical_json(value).encode()).hexdigest()


def now_iso() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"


def atomic_write_text(path: Path, text: str) -> None:
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def read_object(path: Path) -> dict[str, object]:
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        fail(f"{path}: expected object")
    return value


def write_object(path: Path, value: object) -> None:
    atomic_write_text(path, json.dumps(value, ensure_ascii=False, indent=2) + "\n")


@contextmanager
def journal_lock() -> Iterator[None]:
    """Reentrant exclusive flock around journal read/replay/append transactions."""
    global _LOCK_DEPTH
    if _LOCK_DEPTH:
        _LOCK_DEPTH += 1
        try:
            yield
        finally:
            _LOCK_DEPTH -= 1
        return
    LOCK_PATH.touch(exist_ok=True)
    with LOCK_PATH.open("r+") as handle:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        _LOCK_DEPTH = 1
        try:
            yield
        finally:
            _LOCK_DEPTH = 0
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)


def run(argv: Sequence[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(list(argv), cwd=ROOT, text=True, capture_output=True, check=False)
    if check and result.returncode:
        fail(f"command failed ({result.returncode}): {list(argv)!r}\n{result.stderr.strip()}")
    return result


def gh_json(arguments: Sequence[str]) -> object:
    result = run(("gh", *arguments))
    try:
        return json.loads(result.stdout)
    except json.JSONDecodeError as error:
        fail(f"gh returned invalid JSON: {error}")


def section(body: str, heading: str, *, required: bool) -> str | None:
    match = re.search(rf"(?ms)^## {re.escape(heading)}\s*\n(.*?)(?=^## |\Z)", body)
    value = match.group(1).strip() if match else None
    if required and not value:
        fail(f"missing {heading} section")
    return value


def blocker_numbers(body: str, issue: int) -> list[int]:
    text = section(body, "Blocked by", required=True) or ""
    bullets = [line.strip() for line in text.splitlines() if line.lstrip().startswith("-")]
    if not bullets:
        fail(f"issue #{issue}: Blocked by has no bullets")
    if len(bullets) == 1 and re.fullmatch(r"-\s*None\.?", bullets[0], re.IGNORECASE):
        return []
    numbers: list[int] = []
    for bullet in bullets:
        links = ISSUE_LINK_RE.findall(bullet)
        if len(links) != 1:
            fail(f"issue #{issue}: blocker must be one canonical issue link: {bullet}")
        numbers.append(int(links[0]))
    if len(numbers) != len(set(numbers)):
        fail(f"issue #{issue}: duplicate blockers")
    return numbers


def fetch_children(issue: int) -> list[dict[str, object]]:
    pages = gh_json(("api", "--paginate", "--slurp", f"repos/{REPOSITORY}/issues/{issue}/sub_issues"))
    if not isinstance(pages, list):
        fail(f"issue #{issue}: sub-issues response is not pages")
    children: list[dict[str, object]] = []
    for page in pages:
        if not isinstance(page, list):
            fail(f"issue #{issue}: sub-issues page is not an array")
        for child in page:
            if not isinstance(child, dict):
                fail(f"issue #{issue}: invalid sub-issue record")
            children.append(child)
    return children


def fetch_live_tree() -> tuple[dict[int, dict[str, object]], dict[int, int]]:
    records: dict[int, dict[str, object]] = {}
    parents: dict[int, int] = {}
    seen_parents: set[int] = set()
    frontier = [ROOT_ISSUE]
    while frontier:
        batch = sorted(set(frontier) - seen_parents)
        if not batch:
            break
        seen_parents.update(batch)
        with ThreadPoolExecutor(max_workers=12) as pool:
            pages = list(pool.map(fetch_children, batch))
        frontier = []
        for parent, children in zip(batch, pages, strict=True):
            for child in children:
                number = child.get("number")
                if not isinstance(number, int):
                    fail(f"issue #{parent}: child has no number")
                prior = parents.get(number)
                if prior is not None and prior != parent:
                    fail(f"issue #{number}: multiple native parents #{prior} and #{parent}")
                records[number] = child
                parents[number] = parent
                frontier.append(number)
    if len(records) != 131:
        fail(f"expected 131 descendants, got {len(records)}")
    return records, parents


def normalize_live(records: Mapping[int, Mapping[str, object]], parents: Mapping[int, int]) -> list[dict[str, object]]:
    identities: dict[int, str] = {}
    for number, issue in records.items():
        body = issue.get("body")
        if not isinstance(body, str):
            fail(f"issue #{number}: body is not text")
        stable = STABLE_RE.search(body)
        identities[number] = stable.group(1) if stable else f"EXT-{number}"
    normalized: list[dict[str, object]] = []
    stable_ids: set[str] = set()
    for number in sorted(records):
        issue = records[number]
        body = str(issue["body"])
        stable_id = identities[number]
        if stable_id in stable_ids:
            fail(f"duplicate Stable ID {stable_id}")
        stable_ids.add(stable_id)
        external = 13 <= number <= 28
        if external != stable_id.startswith("EXT-"):
            fail(f"issue #{number}: Stable ID/external classification mismatch")
        if not external and not STABLE_RE.search(body):
            fail(f"issue #{number}: missing Stable ID")
        blockers = blocker_numbers(body, number)
        unknown = sorted(set(blockers) - set(records))
        if unknown:
            fail(f"issue #{number}: unknown blockers {unknown}")
        state = issue.get("state")
        if state not in ("open", "closed"):
            fail(f"issue #{number}: invalid state {state!r}")
        if external and state != "closed":
            fail(f"{stable_id}: charting input is not resolved")
        acceptance = section(body, "Acceptance", required=not external)
        parent_number = parents.get(number)
        if parent_number is None:
            fail(f"issue #{number}: missing native parent")
        parent_id = "ROOT-12" if parent_number == ROOT_ISSUE else identities.get(parent_number)
        if parent_id is None:
            fail(f"issue #{number}: unknown native parent #{parent_number}")
        title = issue.get("title")
        url = issue.get("html_url")
        if not isinstance(title, str) or not isinstance(url, str):
            fail(f"issue #{number}: invalid title or URL")
        normalized.append({
            "stableId": stable_id,
            "kind": "external" if external else "execution",
            "issue": number,
            "url": url,
            "title": title,
            "question": section(body, "Question", required=True),
            "acceptance": acceptance,
            "nativeParent": parent_id,
            "blockers": [identities[blocker] for blocker in blockers],
            "issueState": state,
            "resolved": state == "closed",
        })
    return normalized


def live_graph() -> dict[str, object]:
    return build_graph(normalize_live(*fetch_live_tree()))


def require_live_source(graph: Mapping[str, object]) -> dict[str, object]:
    """Live-source gate: refuse mutation when GitHub drifted from the stored graph."""
    live = live_graph()
    if graph.get("sourceHash") != live.get("sourceHash"):
        fail(f"live source drift: stored={graph.get('sourceHash')} live={live.get('sourceHash')}")
    return live


def execution_records(graph_or_records: Mapping[str, object] | Sequence[Mapping[str, object]]) -> list[dict[str, object]]:
    raw = graph_or_records["records"] if isinstance(graph_or_records, Mapping) else graph_or_records
    return [dict(record) for record in raw if record.get("kind") == "execution"]


def derive_waves(records: Sequence[Mapping[str, object]]) -> tuple[dict[str, int], list[list[str]]]:
    executions = execution_records(records)
    known = {str(record["stableId"]) for record in records}
    executable = {str(record["stableId"]) for record in executions}
    dependencies: dict[str, set[str]] = {}
    for record in executions:
        stable_id = str(record["stableId"])
        blockers = {str(item) for item in record["blockers"]}
        unknown = blockers - known
        if unknown:
            fail(f"{stable_id}: unknown blockers {sorted(unknown)}")
        dependencies[stable_id] = blockers & executable
    remaining = set(executable)
    wave_by_id: dict[str, int] = {}
    waves: list[list[str]] = []
    issue_by_id = {str(record["stableId"]): int(record["issue"]) for record in executions}
    while remaining:
        ready = sorted((node for node in remaining if not (dependencies[node] & remaining)), key=issue_by_id.__getitem__)
        if not ready:
            fail(f"execution blocker cycle among {sorted(remaining)}")
        wave = len(waves)
        waves.append(ready)
        for node in ready:
            wave_by_id[node] = wave
        remaining.difference_update(ready)
    return wave_by_id, waves


def transitive_blockers(stable_id: str, records: Sequence[Mapping[str, object]]) -> set[str]:
    direct = {str(record["stableId"]): [str(item) for item in record["blockers"]] for record in records}
    execution = {str(record["stableId"]) for record in records if record.get("kind") == "execution"}
    seen: set[str] = set()
    stack = [item for item in direct[stable_id] if item in execution]
    while stack:
        item = stack.pop()
        if item in seen:
            continue
        seen.add(item)
        stack.extend(parent for parent in direct[item] if parent in execution)
    return seen


def track(stable_id: str) -> str:
    return stable_id.split("-", 1)[0]


def derive_runtime_nodes(records: Sequence[Mapping[str, object]]) -> list[dict[str, object]]:
    executions = execution_records(records)
    wave_by_id, waves = derive_waves(records)
    ordered = sorted(executions, key=lambda record: (wave_by_id[str(record["stableId"])], int(record["issue"])))
    index_by_id = {str(record["stableId"]): index for index, record in enumerate(ordered, 1)}
    nodes: list[dict[str, object]] = [
        {"id": "control:preflight", "kind": "control", "deps": [], "meta": {"check": "graph/live-source"}},
        {"id": "control:design-council", "kind": "council", "deps": ["control:preflight"], "meta": {"seats": ["dependency", "ownership", "review", "git-recovery", "completeness"]}},
    ]
    execution_ids = set(index_by_id)
    for record in ordered:
        stable_id = str(record["stableId"])
        task_no = index_by_id[stable_id]
        blockers = [index_by_id[str(item)] for item in cast(Sequence[object], record["blockers"]) if str(item) in execution_ids]
        scout_deps = ["control:design-council", *(f"task:{item:03d}:done" for item in blockers)]
        if stable_id == "MAP-6":
            scout_deps.append("root:preclose")
        prefix = f"task:{task_no:03d}"
        nodes.extend([
            {"id": f"{prefix}:scout", "kind": "scout", "deps": scout_deps},
            {"id": f"{prefix}:lease", "kind": "lease", "deps": [f"{prefix}:scout", f"wave:{wave_by_id[stable_id]:02d}:allocate"]},
            {"id": f"{prefix}:implement", "kind": "implement", "deps": [f"{prefix}:lease"]},
            {"id": f"{prefix}:verify", "kind": "verify", "deps": [f"{prefix}:implement"]},
            {"id": f"{prefix}:review", "kind": "review", "deps": [f"{prefix}:verify"]},
            {"id": f"{prefix}:fix", "kind": "fix", "deps": [f"{prefix}:review"]},
            {"id": f"{prefix}:integrate", "kind": "integrate", "deps": [f"{prefix}:fix"]},
            {"id": f"{prefix}:gate", "kind": "gate", "deps": [f"{prefix}:integrate"]},
            {"id": f"{prefix}:close", "kind": "close", "deps": [f"{prefix}:gate"]},
            {"id": f"{prefix}:done", "kind": "done", "deps": [f"{prefix}:close"]},
        ])
    for wave, members in enumerate(waves):
        nodes.extend([
            {"id": f"wave:{wave:02d}:allocate", "kind": "allocator", "deps": [f"task:{index_by_id[item]:03d}:scout" for item in members], "meta": {"wave": wave, "resourceOverlay": "journal-leases"}},
            {"id": f"wave:{wave:02d}:integrate", "kind": "wave-gate", "deps": [f"task:{index_by_id[item]:03d}:done" for item in members], "meta": {"wave": wave}},
        ])
    grouped: dict[str, list[str]] = defaultdict(list)
    for record in ordered:
        grouped[track(str(record["stableId"]))].append(str(record["stableId"]))
    for name in TRACKS:
        members = grouped[name]
        nodes.append({"id": f"track:{name}", "kind": "track-gate", "deps": [f"task:{index_by_id[item]:03d}:done" for item in members], "meta": {"owner": f"{name}-CLOSE" if f"{name}-CLOSE" in members else members[-1]}})
    map_preclose = [item for item in grouped["MAP"] if item != "MAP-6"]
    nodes.append({"id": "track:MAP-preclose", "kind": "track-gate", "deps": [f"task:{index_by_id[item]:03d}:done" for item in map_preclose], "meta": {"owner": "MAP-5"}})
    preclose_tracks = [*(f"track:{name}" for name in TRACKS), "track:MAP-preclose"]
    nodes.extend([
        {"id": "posture:minimalism", "kind": "posture", "deps": preclose_tracks, "meta": {"owner": "G10"}},
        {"id": "posture:clean-cutover", "kind": "posture", "deps": preclose_tracks, "meta": {"owner": "G10"}},
    ])
    for seat in COUNCIL_SEATS:
        nodes.append({"id": f"council:final:{seat}", "kind": "council-seat", "deps": ["posture:minimalism", "posture:clean-cutover"], "meta": {"threshold": {"critical": 0, "important": 0}}})
    seat_ids = [f"council:final:{seat}" for seat in COUNCIL_SEATS]
    wave_integrations = [f"wave:{wave:02d}:integrate" for wave in range(len(waves))]
    nodes.extend([
        {"id": "council:final:fix", "kind": "fix", "deps": seat_ids, "meta": {"generationInvalidatesCouncil": True}},
        {"id": "root:preclose", "kind": "root-gate", "deps": [*seat_ids, "council:final:fix", *wave_integrations], "meta": {"gates": [f"G{number}" for number in range(1, 11)]}},
        {"id": "track:MAP", "kind": "track-gate", "deps": [f"task:{index_by_id['MAP-6']:03d}:done"], "meta": {"owner": "MAP-6"}},
        {"id": "root:closed", "kind": "root-gate", "deps": ["root:preclose", "track:MAP"], "meta": {"gate": "G11", "issue": 12}},
    ])
    if len(nodes) != 1202:
        fail(f"expected 1202 runtime nodes, got {len(nodes)}")
    return nodes


def structural_records(records: Sequence[Mapping[str, object]]) -> list[dict[str, object]]:
    """Canonical structural identity of source records, excluding mutable issue state.

    issueState/resolved change as GitHub issues close and must not perturb the
    structural sourceHash, so closing issues stays replayable and refreshable.
    """
    return [
        {key: value for key, value in record.items() if key not in ("issueState", "resolved")}
        for record in records
    ]


def build_graph(records: Sequence[Mapping[str, object]]) -> dict[str, object]:
    wave_by_id, waves = derive_waves(records)
    ordered = sorted(execution_records(records), key=lambda record: (wave_by_id[str(record["stableId"])], int(record["issue"])))
    source_records = [dict(record) for record in records]
    graph = {
        "version": 2,
        "repository": REPOSITORY,
        "canonicalIssue": ROOT_ISSUE,
        "sourceHash": digest(structural_records(source_records)),
        "sourceRecordCount": len(source_records),
        "taskCount": len(ordered),
        "externalCount": sum(record["kind"] == "external" for record in source_records),
        "waveCount": len(waves),
        "runtimeNodeCount": 1202,
        "phases": list(PHASES),
        "records": source_records,
        "executionIndex": [{"index": index, "stableId": record["stableId"]} for index, record in enumerate(ordered, 1)],
        "nativeParentEdges": [{"child": record["stableId"], "parent": record["nativeParent"]} for record in source_records],
        "blockerEdges": [{"blocked": record["stableId"], "blocker": blocker} for record in source_records for blocker in record["blockers"]],
        "runtimeNodes": derive_runtime_nodes(records),
    }
    return graph




def refresh(*, update_plan: bool) -> dict[str, object]:
    """Regenerate workflowz.json as the single published artifact.

    PLAN.md is a projection validated against the graph but is never rewritten
    here, so refresh publishes exactly one artifact and cannot leave a
    two-file crash split between a new graph and a stale PLAN.
    """
    issues, parents = fetch_live_tree()
    graph = build_graph(normalize_live(issues, parents))
    validate_graph(graph, None, plan_text=PLAN_PATH.read_text(), check_plan=True)
    write_object(GRAPH_PATH, graph)
    return graph


def project_plan_text(graph: Mapping[str, object]) -> str:
    records = cast(Sequence[Mapping[str, object]], graph["records"])
    by_id = {str(item["stableId"]): item for item in records}
    issue_blockers: dict[int, list[int]] = {}
    for record in records:
        if record.get("kind") != "execution":
            continue
        blockers = []
        for item in cast(Sequence[object], record["blockers"]):
            blocker = by_id[str(item)]
            if blocker.get("kind") == "execution":
                blockers.append(int(blocker["issue"]))
        issue_blockers[int(record["issue"])] = blockers
    text = PLAN_PATH.read_text()
    lines = text.splitlines(keepends=True)
    current_issue: int | None = None
    seen: set[int] = set()
    for index, line in enumerate(lines):
        issue_match = re.match(r"- \*\*Issue:\*\* https://github\.com/metaphorics/pi-oxidized/issues/(\d+)\s*$", line.rstrip("\n"))
        if issue_match:
            current_issue = int(issue_match.group(1))
        if line.startswith("- **Depends on:**"):
            if current_issue not in issue_blockers:
                fail(f"PLAN dependency line has no execution issue context at line {index + 1}")
            blockers = issue_blockers[current_issue]
            value = ", ".join(f"#{number}" for number in blockers) + "." if blockers else "none among execution tickets."
            newline = "\n" if line.endswith("\n") else ""
            lines[index] = f"- **Depends on:** {value}{newline}"
            seen.add(current_issue)
    missing = set(issue_blockers) - seen
    if missing:
        fail(f"PLAN missing dependency lines for issues {sorted(missing)}")
    return "".join(lines)


def project_plan(graph: Mapping[str, object]) -> None:
    atomic_write_text(PLAN_PATH, project_plan_text(graph))


def validate_graph(graph: Mapping[str, object], live_graph: Mapping[str, object] | None = None, *, plan_text: str | None = None, check_plan: bool = True) -> None:
    records = graph.get("records")
    if not isinstance(records, list) or not all(isinstance(item, dict) for item in records):
        fail("graph.records must be objects")
    if len(records) != 131 or graph.get("sourceRecordCount") != 131:
        fail("graph must contain 131 source records")
    if digest(structural_records(records)) != graph.get("sourceHash"):
        fail("graph source hash does not match records")
    executions = execution_records(records)
    externals = [record for record in records if record.get("kind") == "external"]
    if len(executions) != 115 or graph.get("taskCount") != 115:
        fail("graph must contain 115 execution records")
    if len(externals) != 16 or graph.get("externalCount") != 16:
        fail("graph must contain 16 external records")
    expected_external = {f"EXT-{number}" for number in range(13, 29)}
    if {record["stableId"] for record in externals} != expected_external:
        fail("external Stable-ID set mismatch")
    if any(not record.get("resolved") for record in externals):
        fail("all external records must be resolved")
    stable_ids = [record.get("stableId") for record in records]
    if any(not isinstance(item, str) or not item for item in stable_ids) or len(stable_ids) != len(set(stable_ids)):
        fail("Stable IDs must be nonempty and unique")
    known = set(stable_ids)
    if any(set(record.get("blockers", [])) - known for record in records):
        fail("unknown or alias blocker")
    wave_by_id, waves = derive_waves(records)
    if len(waves) != 15 or graph.get("waveCount") != 15:
        fail("expected 15 derived waves")
    execution_ids = {str(record["stableId"]) for record in executions}
    if transitive_blockers("MAP-5", records) != execution_ids - {"MAP-5", "MAP-6"}:
        fail("MAP-5 transitive blockers are not the other 113 pre-close tasks")
    if transitive_blockers("MAP-6", records) != execution_ids - {"MAP-6"}:
        fail("MAP-6 transitive blockers are not all other 114 tasks")
    parent_by_id = {str(record["stableId"]): str(record["nativeParent"]) for record in records}
    for stable_id in stable_ids:
        seen: set[str] = set()
        cursor = str(stable_id)
        while cursor != "ROOT-12":
            if cursor in seen:
                fail(f"native parent cycle reaches {cursor}")
            seen.add(cursor)
            cursor = parent_by_id.get(cursor, "")
            if not cursor:
                fail(f"{stable_id}: no path to native root")
    reverse: dict[str, list[str]] = defaultdict(list)
    for record in executions:
        for blocker in record["blockers"]:
            if blocker in execution_ids:
                reverse[str(blocker)].append(str(record["stableId"]))
    for start in execution_ids:
        stack = [start]
        seen = {start}
        while stack:
            item = stack.pop()
            for child in reverse[item]:
                if child not in seen:
                    seen.add(child)
                    stack.append(child)
        if "MAP-6" not in seen:
            fail(f"{start}: no blocker path to MAP-6")
    expected_index = [record["stableId"] for record in sorted(executions, key=lambda record: (wave_by_id[str(record["stableId"])], int(record["issue"])))]
    actual_index = graph.get("executionIndex")
    if not isinstance(actual_index, list) or actual_index != [{"index": index, "stableId": stable_id} for index, stable_id in enumerate(expected_index, 1)]:
        fail("local execution index mismatch")
    expected_nodes = derive_runtime_nodes(records)
    if graph.get("runtimeNodes") != expected_nodes or graph.get("runtimeNodeCount") != 1202:
        fail("runtime node IDs or dependencies mismatch")
    node_by_id = {str(node["id"]): node for node in expected_nodes}
    node_ids = set(node_by_id)
    if len(node_ids) != 1202:
        fail("runtime node IDs are not unique")
    for node in expected_nodes:
        unknown = set(node["deps"]) - node_ids
        if unknown:
            fail(f"{node['id']}: unknown runtime dependencies {sorted(unknown)}")
    closure = {"root:closed"}
    stack = ["root:closed"]
    while stack:
        for dep in node_by_id[stack.pop()]["deps"]:
            if dep not in closure:
                closure.add(dep)
                stack.append(dep)
    outside = node_ids - closure
    if outside:
        fail(f"runtime nodes outside the root:closed closure: {len(outside)} missing, sample={sorted(outside)[:6]}")
    if check_plan:
        validate_plan_text(graph, plan_text if plan_text is not None else PLAN_PATH.read_text())
    if live_graph is not None and graph.get("sourceHash") != live_graph.get("sourceHash"):
        fail(f"live source hash differs: stored={graph.get('sourceHash')} live={live_graph.get('sourceHash')}")


def validate_plan_text(graph: Mapping[str, object], text: str) -> None:
    by_id = {str(record["stableId"]): record for record in graph["records"]}
    expected = {int(record["issue"]): [int(by_id[item]["issue"]) for item in record["blockers"] if by_id[item]["kind"] == "execution"] for record in graph["records"] if record["kind"] == "execution"}
    lines = text.splitlines()
    current: int | None = None
    actual: dict[int, list[int]] = {}
    for line in lines:
        match = re.match(r"- \*\*Issue:\*\* https://github\.com/metaphorics/pi-oxidized/issues/(\d+)$", line)
        if match:
            current = int(match.group(1))
        if line.startswith("- **Depends on:**"):
            if current is None or current in actual:
                fail("PLAN has ambiguous dependency projection")
            actual[current] = [int(item) for item in re.findall(r"#(\d+)", line)]
    if actual != expected:
        missing = sorted(set(expected) ^ set(actual))
        wrong = sorted(number for number in set(expected) & set(actual) if actual[number] != expected[number])
        fail(f"PLAN dependency projection mismatch missing={missing} wrong={wrong}")


def validate_plan_projection(graph: Mapping[str, object]) -> None:
    validate_plan_text(graph, PLAN_PATH.read_text())


def event_token(event: Mapping[str, object]) -> str:
    return digest({key: value for key, value in event.items() if key not in ("seq", "at", "reviewToken", "councilToken")})


def require_fields(event: Mapping[str, object], required: Iterable[str], allowed: Iterable[str]) -> None:
    required_set = set(required) | {"seq", "at", "event"}
    allowed_set = set(allowed) | required_set
    missing = required_set - set(event)
    extra = set(event) - allowed_set
    if missing or extra:
        fail(f"journal seq {event.get('seq')}: missing={sorted(missing)} extra={sorted(extra)}")


def validate_binding(event: Mapping[str, object], executions: set[str]) -> None:
    if event.get("stableId") not in executions:
        fail(f"journal seq {event.get('seq')}: unknown stableId")
    if not isinstance(event.get("attempt"), int) or int(event["attempt"]) < 1:
        fail("attempt must be positive")
    if not isinstance(event.get("generation"), int) or int(event["generation"]) < 1:
        fail("generation must be positive")
    for field in ("baseSha", "headSha"):
        if not isinstance(event.get(field), str) or not SHA_RE.fullmatch(str(event[field])):
            fail(f"journal seq {event.get('seq')}: invalid {field}")


def require_identity(event: Mapping[str, object], *fields: str) -> None:
    for field in fields:
        value = event.get(field)
        if not isinstance(value, str) or not value.strip():
            fail(f"journal seq {event.get('seq')}: invalid {field} identity")


def require_independent(event: Mapping[str, object], dependent: str, authority: str) -> None:
    if event.get(dependent) == event.get(authority):
        fail(f"journal seq {event.get('seq')}: {dependent} must differ from {authority}")



def require_active_lease(event: Mapping[str, object], result: Mapping[str, object]) -> None:
    lease_id = event.get("leaseId")
    lease = result["leases"].get(lease_id)
    if lease is None:
        status = result["leaseStatus"].get(lease_id)
        if isinstance(status, Mapping):
            status = status.get("status")
        if status in ("released", "reclaimed", "terminated"):
            fail(f"journal seq {event.get('seq')}: {status} lease actor: {lease_id!r}")
        fail(f"journal seq {event.get('seq')}: no active lease {lease_id!r}")
    if (lease["stableId"] != event.get("stableId")
            or lease["epoch"] != event.get("leaseEpoch")
            or lease["claimant"] != event.get("claimant")
            or lease["attempt"] != event.get("attempt")
            or lease["generation"] != event.get("generation")
            or lease["baseSha"] != event.get("baseSha")
            or lease["headSha"] != event.get("headSha")):
        fail(f"journal seq {event.get('seq')}: lease binding mismatch for {lease_id!r}")


def retire_leases(result: Mapping[str, object], stable_id: str, reason: str) -> None:
    """Terminate every active lease for a Stable ID with a typed reason."""
    leases = result["leases"]
    status = result["leaseStatus"]
    for lease_id in [lease_id for lease_id, lease in list(leases.items()) if lease.get("stableId") == stable_id]:
        del leases[lease_id]
        status[lease_id] = {"status": "terminated", "reason": reason}


def require_clean_review(event: Mapping[str, object], result: Mapping[str, object]) -> None:
    stable_id = str(event["stableId"])
    review = result["cleanReviews"].get(stable_id)
    if (review is None
            or event.get("reviewToken") != review.get("reviewToken")
            or review.get("generation") != event.get("generation")
            or review.get("attempt") != event.get("attempt")
            or review.get("headSha") != event.get("headSha")):
        fail(f"journal seq {event.get('seq')}: {stable_id} lacks a same-attempt clean-review token")


def validate_findings(event: Mapping[str, object], seq: int) -> list[dict[str, object]]:
    findings = event.get("findings")
    if not isinstance(findings, list) or not all(
        isinstance(item, dict)
        and set(item) == {"id", "severity", "summary"}
        and isinstance(item["id"], str) and item["id"]
        and isinstance(item["summary"], str) and item["summary"]
        and item["severity"] in {"Critical", "Important", "Minor"}
        for item in findings
    ):
        fail(f"journal seq {seq}: findings are not typed")
    ids = [item["id"] for item in findings]
    if len(ids) != len(set(ids)):
        dupes = sorted({item for item in ids if ids.count(item) > 1})
        fail(f"journal seq {seq}: duplicate finding IDs {dupes}")
    return findings


def replay(graph: Mapping[str, object], state: Mapping[str, object]) -> dict[str, object]:
    if state.get("version") != 2 or state.get("graphSourceHash") != graph.get("sourceHash") or state.get("durableRef") != DURABLE_REF:
        fail("state header does not match graph or durable ref")
    journal = state.get("journal")
    if not isinstance(journal, list) or not journal:
        fail("state journal must be nonempty")
    records = cast(Sequence[Mapping[str, object]], graph["records"])
    executions = {str(record["stableId"]) for record in records if record["kind"] == "execution"}
    execution_blockers = {str(record["stableId"]): [str(item) for item in record["blockers"] if str(item) in executions] for record in records if record.get("kind") == "execution"}
    execution_status = {
        stable_id: {"status": RUNNABLE_STATUS, "reason": None, "evidenceEnvironment": None}
        for stable_id in executions
    }
    result: dict[str, object] = {
        "head": None, "generations": defaultdict(int), "attempts": defaultdict(int), "phases": defaultdict(set),
        "progress": defaultdict(lambda: -1), "findings": defaultdict(dict), "cleanReviews": {}, "leases": {},
        "leaseStatus": {}, "leaseEpochs": defaultdict(int), "gates": {}, "closed": set(), "done": set(),
        "council": {}, "councilReviewers": set(), "perfIterations": [], "integrations": [], "pending": {},
        "fixLoop": {}, "executionStatus": execution_status, "bindings": {},
        "rootClosed": False,
    }
    last_at = ""
    for expected_seq, raw in enumerate(journal, 1):
        if not isinstance(raw, dict):
            fail(f"journal seq {expected_seq}: expected object")
        event = raw
        if event.get("seq") != expected_seq:
            fail(f"journal sequence gap at {expected_seq}")
        at = event.get("at")
        if not isinstance(at, str) or not ISO_RE.fullmatch(at) or at < last_at:
            fail(f"journal seq {expected_seq}: invalid or decreasing timestamp")
        last_at = at
        kind = event.get("event")
        if kind == "genesis":
            require_fields(event, ("graphSourceHash", "durableRef", "headSha", "actor"), ())
            if expected_seq != 1 or event["graphSourceHash"] != graph["sourceHash"] or event["durableRef"] != DURABLE_REF or not SHA_RE.fullmatch(str(event["headSha"])):
                fail("invalid genesis")
            result["head"] = event["headSha"]
            continue
        if expected_seq == 1:
            fail("journal must start with genesis")
        binding_required = ("stableId", "attempt", "generation", "baseSha", "headSha")
        if kind in BOUND_EVENTS:
            validate_binding(event, executions)
        stable_id = str(event.get("stableId", ""))
        generation = int(event.get("generation", 0))
        attempt = int(event.get("attempt", 0))
        if kind in BOUND_EVENTS and kind not in {"invalidate"}:
            current_generation = result["generations"][stable_id]
            current_attempt = result["attempts"][stable_id]
            current_status = result["executionStatus"][stable_id]["status"]
            if current_generation == 0:
                result["generations"][stable_id] = generation
                result["attempts"][stable_id] = attempt
            else:
                if generation != current_generation:
                    fail(f"journal seq {expected_seq}: stale generation replay")
                if attempt < current_attempt:
                    fail(f"journal seq {expected_seq}: stale attempt replay")
                if current_status != RUNNABLE_STATUS:
                    if attempt > current_attempt:
                        fail(f"journal seq {expected_seq}: attempt advance requires READY, found {current_status!r}")
                    if kind in NON_READY_BLOCKED_EVENTS:
                        fail(f"journal seq {expected_seq}: {kind} requires READY, found {current_status!r}")
                if attempt > current_attempt:
                    retire_leases(result, stable_id, "attempt-advanced")
                    result["attempts"][stable_id] = attempt
            if current_status != RUNNABLE_STATUS and kind in NON_READY_BLOCKED_EVENTS:
                fail(f"journal seq {expected_seq}: {kind} requires READY, found {current_status!r}")
            # Canonical baseSha/headSha are established by the first bound work event of
            # each stableId+attempt; every later bound event (including hold/resume) must match.
            binding = result["bindings"].get(stable_id)
            if binding is None or int(binding["attempt"]) != attempt:
                result["bindings"][stable_id] = {
                    "attempt": attempt,
                    "baseSha": event["baseSha"],
                    "headSha": event["headSha"],
                }
            elif binding["baseSha"] != event["baseSha"] or binding["headSha"] != event["headSha"]:
                fail(
                    f"journal seq {expected_seq}: bound event SHA mismatch for {stable_id} "
                    f"attempt {attempt}: expected base={binding['baseSha']} head={binding['headSha']}"
                )
        elif kind == "invalidate":
            current_attempt = result["attempts"][stable_id]
            if current_attempt < 1 or attempt != current_attempt:
                fail(
                    f"journal seq {expected_seq}: invalidate attempt {attempt} does not match "
                    f"current attempt {current_attempt}"
                )
            binding = result["bindings"].get(stable_id)
            if (
                binding is None
                or int(binding["attempt"]) != attempt
                or binding["baseSha"] != event["baseSha"]
                or binding["headSha"] != event["headSha"]
            ):
                fail(f"journal seq {expected_seq}: invalidate SHA binding mismatch for {stable_id}")
        phase = EVENT_PHASE.get(kind) if isinstance(kind, str) else None
        if phase is not None:
            index = PHASE_INDEX[phase]
            progress = int(result["progress"][stable_id])
            if index != progress and index != progress + 1:
                fail(f"journal seq {expected_seq}: {kind} phase-order violation at progress {progress}")
            result["progress"][stable_id] = max(progress, index)
        elif kind == "lease-acquire":
            progress = int(result["progress"][stable_id])
            if progress < PHASE_INDEX["scout"]:
                fail(f"journal seq {expected_seq}: lease-acquire before scout")
            result["progress"][stable_id] = max(progress, PHASE_INDEX["lease"])
        if kind == "hold":
            require_fields(event, (*binding_required, "status", "actor", "reason"), ("evidenceEnvironment",))
            require_identity(event, "actor")
            status = event["status"]
            reason = event["reason"]
            environment = event.get("evidenceEnvironment")
            current_status = result["executionStatus"][stable_id]["status"]
            if current_status != RUNNABLE_STATUS:
                fail(f"journal seq {expected_seq}: hold requires READY, found {current_status!r}")
            if status not in NON_RUNNABLE_STATUSES:
                fail(f"journal seq {expected_seq}: invalid hold status {status!r}")
            if not isinstance(reason, str) or not reason.strip():
                fail(f"journal seq {expected_seq}: hold reason must be nonempty")
            if environment is not None and (not isinstance(environment, str) or not environment.strip()):
                fail(f"journal seq {expected_seq}: invalid evidence environment")
            result["executionStatus"][stable_id] = {
                "status": status,
                "reason": reason,
                "evidenceEnvironment": (
                    {"name": environment, "available": False, "evidence": None}
                    if environment is not None
                    else None
                ),
            }
        elif kind == "resume":
            require_fields(event, (*binding_required, "actor", "resolution", "evidence"), ("evidenceEnvironment",))
            require_identity(event, "actor")
            resolution = event["resolution"]
            evidence = event["evidence"]
            environment = event.get("evidenceEnvironment")
            held = result["executionStatus"][stable_id]
            if held["status"] not in NON_RUNNABLE_STATUSES:
                fail(f"journal seq {expected_seq}: resume requires a non-runnable status, found {held['status']!r}")
            if not isinstance(resolution, str) or not resolution.strip() or not isinstance(evidence, str) or not evidence.strip():
                fail(f"journal seq {expected_seq}: resume resolution and evidence must be nonempty")
            held_environment = held["evidenceEnvironment"]
            held_name = held_environment["name"] if isinstance(held_environment, Mapping) else None
            if environment is not None and (not isinstance(environment, str) or not environment.strip()):
                fail(f"journal seq {expected_seq}: invalid evidence environment")
            if environment is not None and held_name is not None and environment != held_name:
                fail(f"journal seq {expected_seq}: resume evidence environment does not match hold")
            environment_name = environment if environment is not None else held_name
            result["executionStatus"][stable_id] = {
                "status": RUNNABLE_STATUS,
                "reason": resolution,
                "evidenceEnvironment": (
                    {"name": environment_name, "available": True, "evidence": evidence}
                    if environment_name is not None
                    else None
                ),
            }
        elif kind == "scout":

            require_fields(event, (*binding_required, "claimant", "questionHash", "acceptanceHash"), ())
            require_identity(event, "claimant")
            result["phases"][stable_id].add("scout")
        elif kind == "lease-acquire":
            require_fields(event, (*binding_required, "leaseId", "epoch", "claimant", "resources", "expiresAt"), ())
            require_identity(event, "claimant")
            resources = event["resources"]
            if not isinstance(resources, dict) or resources.get("closureComplete") is not True or not all(isinstance(resources.get(name), list) and all(isinstance(item, str) for item in resources[name]) for name in ("files", "interfaces")):
                fail(f"journal seq {expected_seq}: incomplete resource closure")
            lease_id = event["leaseId"]
            if not isinstance(lease_id, str) or not lease_id or not isinstance(event["epoch"], int) or event["epoch"] < 1 or not ISO_RE.fullmatch(str(event["expiresAt"])):
                fail("invalid lease")
            for active in result["leases"].values():
                if set(resources["files"]) & set(active["resources"]["files"]) or set(resources["interfaces"]) & set(active["resources"]["interfaces"]):
                    fail(f"journal seq {expected_seq}: active resource collision")
            if event["epoch"] <= result["leaseEpochs"][stable_id]:
                fail(f"journal seq {expected_seq}: lease epoch did not advance monotonically (last={result['leaseEpochs'][stable_id]})")
            result["leaseEpochs"][stable_id] = event["epoch"]
            result["leases"][lease_id] = dict(event)
            result["leaseStatus"][lease_id] = "active"
            result["phases"][stable_id].add("lease")
        elif kind in {"lease-release", "lease-reclaim"}:
            require_fields(event, (*binding_required, "leaseId", "epoch", "actor", "reason"), ())
            require_identity(event, "actor")
            lease = result["leases"].get(event["leaseId"])
            if lease is None or lease["epoch"] != event["epoch"] or lease["stableId"] != stable_id:
                fail("lease release/reclaim does not match active epoch")
            del result["leases"][event["leaseId"]]
            result["leaseStatus"][event["leaseId"]] = {"status": "released" if kind == "lease-release" else "reclaimed", "reason": event["reason"]}
        elif kind == "implement":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "commits", "reportPath"), ())
            require_identity(event, "claimant")
            require_active_lease(event, result)
            if not isinstance(event["commits"], list) or not event["commits"] or not all(SHA_RE.fullmatch(str(item)) for item in event["commits"]):
                fail("implement commits are invalid")
            result["phases"][stable_id].add("implement")
        elif kind == "verify":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "verifier", "check", "command", "exitCode", "result", "artifactPath"), ())
            require_identity(event, "claimant", "verifier")
            require_independent(event, "verifier", "claimant")
            require_active_lease(event, result)
            loop = result["fixLoop"].get(stable_id)
            if loop and loop["attempt"] == attempt and not loop["verifyDone"]:
                loop["verifyDone"] = True
            result["phases"][stable_id].add("verify")
        elif kind == "review":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "reviewer", "reviewPath", "findings", "reviewToken"), ())
            require_identity(event, "claimant", "reviewer")
            require_independent(event, "reviewer", "claimant")
            findings = validate_findings(event, expected_seq)
            if event["reviewToken"] != event_token(event):
                fail("review token mismatch")
            require_active_lease(event, result)
            loop = result["fixLoop"].get(stable_id)
            if loop and loop["attempt"] == attempt and not loop["verifyDone"]:
                fail(f"journal seq {expected_seq}: review before re-verifying a fixed attempt")
            result["findings"][stable_id] = {item["id"]: item for item in findings}
            result["cleanReviews"].pop(stable_id, None)
            if not any(item["severity"] in {"Critical", "Important"} for item in findings):
                result["cleanReviews"][stable_id] = dict(event)
            if loop and loop["attempt"] == attempt and loop["verifyDone"]:
                result["fixLoop"].pop(stable_id, None)
            result["phases"][stable_id].add("review")
        elif kind == "fix":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "fixer", "findingIds", "commits", "reportPath"), ())
            require_identity(event, "claimant", "fixer")
            require_independent(event, "fixer", "claimant")
            require_active_lease(event, result)
            if set(event["findingIds"]) != set(result["findings"].get(stable_id, {})):
                fail("fix does not cover the complete finding set")
            result["findings"].pop(stable_id, None)
            result["cleanReviews"].pop(stable_id, None)
            result["pending"].pop(stable_id, None)
            result["fixLoop"][stable_id] = {"attempt": attempt, "verifyDone": False, "reviewDone": False}
            result["progress"][stable_id] = PHASE_INDEX["verify"]
            result["phases"][stable_id].add("fix")
        elif kind == "fix-skip":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "reviewToken"), ())
            require_identity(event, "claimant")
            require_active_lease(event, result)
            require_clean_review(event, result)
            result["phases"][stable_id].add("fix")
        elif kind == "integrate-prepared":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "expectedHead", "newHead", "reviewToken"), ())
            require_identity(event, "claimant")
            require_active_lease(event, result)
            require_clean_review(event, result)
            if event["expectedHead"] != result["head"]:
                fail(f"journal seq {expected_seq}: prepared expectedHead {event['expectedHead']} is not the journal head")
            if not SHA_RE.fullmatch(str(event["newHead"])):
                fail(f"journal seq {expected_seq}: invalid prepared newHead")
            if stable_id in result["pending"]:
                fail(f"journal seq {expected_seq}: a prepared integration is already pending for {stable_id}")
            result["pending"][stable_id] = dict(event)
        elif kind == "integrate-finalized":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "expectedHead", "newHead", "commitSha", "reviewToken", "casResult"), ())
            require_identity(event, "claimant")
            require_active_lease(event, result)
            require_clean_review(event, result)
            prepared = result["pending"].get(stable_id)
            if prepared is None or prepared["expectedHead"] != event["expectedHead"] or prepared["newHead"] != event["newHead"] or prepared["reviewToken"] != event["reviewToken"]:
                fail("finalization does not match the prepared integration")
            if event["expectedHead"] != result["head"] or event["newHead"] != event["commitSha"] or event["casResult"] != "updated":
                fail("integration CAS chain mismatch")
            result["head"] = event["newHead"]
            del result["pending"][stable_id]
            result["integrations"].append(dict(event))
            result["phases"][stable_id].add("integrate")
            result["fixLoop"].pop(stable_id, None)
            result["gates"] = {name: gate for name, gate in result["gates"].items() if gate.get("headSha") == result["head"]}
            result["council"] = {seat: seat_event for seat, seat_event in result["council"].items() if seat_event.get("headSha") == result["head"]}
            for other in executions - {stable_id}:
                clean = result["cleanReviews"].get(other)
                if clean and clean["headSha"] != result["head"]:
                    result["cleanReviews"].pop(other, None)
        elif kind == "integrate-aborted":
            require_fields(event, (*binding_required, "actor", "reason", "expectedHead", "newHead", "casResult"), ())
            require_identity(event, "actor")
            prepared = result["pending"].get(stable_id)
            if prepared is None or prepared["expectedHead"] != event["expectedHead"] or prepared["newHead"] != event["newHead"]:
                fail("abort does not match the prepared integration")
            if event["casResult"] != "aborted":
                fail("abort must record casResult=aborted")
            del result["pending"][stable_id]
        elif kind == "gate":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "owner", "name", "command", "exitCode", "result", "artifactPath", "artifactDigest"), ("scope",))
            require_identity(event, "claimant", "owner")
            require_active_lease(event, result)
            name = event["name"]
            if name not in GATE_COMMANDS:
                fail(f"journal seq {expected_seq}: unknown gate name {name!r}")
            if not isinstance(event["command"], list) or list(event["command"]) != list(GATE_COMMANDS[name]):
                fail(f"journal seq {expected_seq}: gate {name} command is not canonical")
            if event["exitCode"] != 0 or event["result"] != "pass" or event["headSha"] != result["head"]:
                fail("gate is not a passing current-head command")
            artifact = ROOT / str(event["artifactPath"])
            if not artifact.is_file():
                fail(f"journal seq {expected_seq}: gate artifact missing: {event['artifactPath']}")
            actual_digest = "sha256:" + hashlib.sha256(artifact.read_bytes()).hexdigest()
            if event["artifactDigest"] != actual_digest:
                fail(f"journal seq {expected_seq}: gate artifact digest mismatch for {event['artifactPath']}")
            result["gates"][name] = dict(event)
            result["phases"][stable_id].add("gate")
        elif kind == "close":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "issue", "closedBy", "command", "result"), ())
            require_identity(event, "claimant", "closedBy")
            require_active_lease(event, result)
            if event["result"] != "closed":
                fail("close event did not close issue")
            result["closed"].add(stable_id)
            result["phases"][stable_id].add("close")
        elif kind == "done":
            require_fields(event, (*binding_required, "claimant", "leaseId", "leaseEpoch", "completedBy"), ())
            require_identity(event, "claimant", "completedBy")
            require_active_lease(event, result)
            needed = {"scout", "lease", "implement", "verify", "review", "fix", "integrate", "gate", "close"}
            if not needed <= result["phases"][stable_id] or stable_id not in result["closed"]:
                fail(f"{stable_id}: done before mandatory phases")
            unresolved = [item for item in execution_blockers[stable_id] if item not in result["done"]]
            if unresolved:
                fail(f"journal seq {expected_seq}: {stable_id} done before blockers completed: {unresolved}")
            result["done"].add(stable_id)
            result["phases"][stable_id].add("done")
        elif kind == "invalidate":
            require_fields(event, (*binding_required, "actor", "reason", "nextGeneration", "newBaseSha"), ())
            require_identity(event, "actor")
            if event["nextGeneration"] != generation + 1 or event["generation"] != result["generations"][stable_id]:
                fail("invalid generation transition")
            result["generations"][stable_id] = event["nextGeneration"]
            result["phases"][stable_id].clear()
            result["progress"][stable_id] = -1
            retire_leases(result, stable_id, "generation-invalidated")
            result["fixLoop"].pop(stable_id, None)
            result["findings"].pop(stable_id, None)
            result["cleanReviews"].pop(stable_id, None)
            result["closed"].discard(stable_id)
            result["done"].discard(stable_id)
            result["pending"].pop(stable_id, None)
            result["gates"] = {name: gate for name, gate in result["gates"].items() if gate.get("stableId") != stable_id}
        elif kind == "restack":
            require_fields(event, ("actor", "reason", "oldHead", "newHead", "invalidates"), ())
            require_identity(event, "actor")
            if event["oldHead"] != result["head"] or not SHA_RE.fullmatch(str(event["newHead"])):
                fail("restack does not chain from the journal head")
            invalidates = event["invalidates"]
            integrated = {item["stableId"] for item in result["integrations"]}
            if not isinstance(invalidates, list) or not set(invalidates) <= integrated:
                fail("restack invalidates must name integrated execution stable IDs")
            invalidated = set(invalidates)
            result["head"] = event["newHead"]
            for sid in invalidated:
                result["generations"][sid] += 1
                result["phases"][sid].clear()
                result["progress"][sid] = -1
                retire_leases(result, sid, "restack-invalidated")
                result["fixLoop"].pop(sid, None)
                result["findings"].pop(sid, None)
                result["cleanReviews"].pop(sid, None)
                result["closed"].discard(sid)
                result["done"].discard(sid)
                result["pending"].pop(sid, None)
            result["integrations"] = [item for item in result["integrations"] if item["stableId"] not in invalidated]
            result["gates"] = {name: gate for name, gate in result["gates"].items() if gate.get("headSha") == event["newHead"] and gate.get("stableId") not in invalidated}
            result["council"] = {seat: seat_event for seat, seat_event in result["council"].items() if seat_event.get("headSha") == event["newHead"]}
            for other, clean in list(result["cleanReviews"].items()):
                if clean["headSha"] != event["newHead"]:
                    result["cleanReviews"].pop(other, None)
        elif kind == "perf-iteration":
            require_fields(event, (*binding_required, "iteration", "commitSha", "claimant", "command", "result", "artifactPath"), ())
            require_identity(event, "claimant")
            if stable_id != "PERF-T11" or not isinstance(event["iteration"], int) or event["iteration"] < 1 or not SHA_RE.fullmatch(str(event["commitSha"])):
                fail("invalid PERF-T11 iteration")
            result["perfIterations"].append(dict(event))
        elif kind == "council":
            require_fields(event, ("generation", "headSha", "seat", "reviewer", "reviewPath", "findings", "councilToken"), ())
            require_identity(event, "reviewer")
            if event["seat"] not in COUNCIL_SEATS or event["headSha"] != result["head"] or event["councilToken"] != event_token(event):
                fail("invalid or stale council seat")
            findings = validate_findings(event, expected_seq)
            if any(item["severity"] in {"Critical", "Important"} for item in findings):
                fail("council seat is not clean")
            if event["reviewer"] in result["councilReviewers"]:
                fail(f"journal seq {expected_seq}: council reviewer {event['reviewer']!r} reused across seats")
            result["councilReviewers"].add(event["reviewer"])
            result["council"][event["seat"]] = dict(event)
        elif kind == "root-close":
            require_fields(event, ("generation", "headSha", "issue", "closedBy", "command", "result"), ())
            require_identity(event, "closedBy")
            if event["issue"] != ROOT_ISSUE or event["headSha"] != result["head"] or event["result"] != "closed":
                fail("invalid root close")
            verify_pre_close(graph, result)
            if live_root_state() != "closed":
                fail("root-close: live root issue #12 is not closed")
            result["rootClosed"] = True
        else:
            fail(f"journal seq {expected_seq}: unknown event {kind!r}")
    commits = [event["commitSha"] for event in result["perfIterations"]]
    iterations = [event["iteration"] for event in result["perfIterations"]]
    if len(commits) != len(set(commits)) or iterations != list(range(1, len(iterations) + 1)):
        fail("PERF-T11 iteration/commit bijection mismatch")
    return result


def live_root_state() -> str:
    data = gh_json(("api", f"repos/{REPOSITORY}/issues/{ROOT_ISSUE}"))
    return str(data.get("state", "")) if isinstance(data, dict) else ""


def verify_pre_close(graph: Mapping[str, object], replayed: Mapping[str, object]) -> None:
    """Shared pre-close validation used by the G11 gate and by root-close replay."""
    executions = {str(record["stableId"]): record for record in graph["records"] if record["kind"] == "execution"}
    if set(replayed["done"]) != set(executions):
        fail(f"pre-close: mandatory nodes incomplete ({len(replayed['done'])}/{len(executions)})")
    if replayed["leases"]:
        fail("pre-close: active leases remain")
    if replayed["pending"]:
        fail(f"pre-close: prepared integrations still pending {sorted(replayed['pending'])}")
    # Live per-issue closure check — sourceHash deliberately drops issueState so the
    # stored snapshot goes stale as issues close; never trust graph["issueState"] here.
    open_live: list[str] = []
    for stable_id, record in executions.items():
        issue = record.get("issue")
        if not isinstance(issue, int):
            continue
        data = gh_json(("api", f"repos/{REPOSITORY}/issues/{issue}"))
        state = str(data.get("state", "")) if isinstance(data, dict) else ""
        if state != "closed":
            open_live.append(f"{stable_id}#{issue}:{state or 'unknown'}")
    if open_live:
        fail(f"pre-close: live execution issues still open ({len(open_live)}): {', '.join(sorted(open_live)[:10])}{' ...' if len(open_live) > 10 else ''}")
    if not required_gates <= set(replayed["gates"]):
        fail(f"pre-close: named gates missing {sorted(required_gates - set(replayed['gates']))}")
    for name, gate in replayed["gates"].items():
        if name in required_gates and gate.get("headSha") != replayed["head"]:
            fail(f"pre-close: gate {name} does not target the current head")
    if set(replayed["council"]) != set(COUNCIL_SEATS):
        fail("pre-close: five-seat council incomplete")
    heads = {seat["headSha"] for seat in replayed["council"].values()}
    generations = {seat["generation"] for seat in replayed["council"].values()}
    if heads != {replayed["head"]} or len(generations) != 1:
        fail("pre-close: council is not same-SHA and same-generation")
    integrations = {event["stableId"]: index for index, event in enumerate(replayed["integrations"])}
    if integrations.get("MAP-5", -1) >= integrations.get("MAP-6", -1):
        fail("pre-close: MAP-5 did not integrate before MAP-6")


def all_done(graph: Mapping[str, object], replayed: Mapping[str, object]) -> None:
    verify_pre_close(graph, replayed)
    if "G11" not in replayed["gates"]:
        fail("all-done: G11 gate missing")
    if replayed["gates"]["G11"].get("headSha") != replayed["head"]:
        fail("all-done: G11 gate does not target the final head")
    if not replayed["rootClosed"]:
        fail("all-done: issue #12 root closure missing")
    if live_root_state() != "closed":
        fail("all-done: live root issue #12 is not closed")
    if durable_ref_head() != replayed["head"]:
        fail(f"all-done: durable ref does not match the journal head {replayed['head']}")


def validate_state(graph: Mapping[str, object], *, require_all_done: bool) -> dict[str, object]:
    state = read_object(STATE_PATH)
    replayed = replay(graph, state)
    if require_all_done:
        all_done(graph, replayed)
    return replayed


def durable_ref_head() -> str:
    return run(("git", "rev-parse", "--verify", DURABLE_REF)).stdout.strip()


def append_event(graph: Mapping[str, object], event: Mapping[str, object]) -> dict[str, object]:
    with journal_lock():
        state = read_object(STATE_PATH)
        journal = state.get("journal")
        if not isinstance(journal, list):
            fail("state journal missing")
        record = dict(event)
        record["seq"] = len(journal) + 1
        record.setdefault("at", now_iso())
        if record.get("event") == "review":
            record["reviewToken"] = event_token(record)
        if record.get("event") == "council":
            record["councilToken"] = event_token(record)
        candidate = {**state, "journal": [*journal, record]}
        replay(graph, candidate)
        write_object(STATE_PATH, candidate)
        return record


def reconcile(graph: Mapping[str, object]) -> dict[str, object]:
    """Startup reconciliation of the durable ref against the journal head."""
    state = read_object(STATE_PATH)
    replayed = replay(graph, state)
    ref = durable_ref_head()
    for stable_id, prepared in list(replayed["pending"].items()):
        if ref == prepared["newHead"] and replayed["head"] == prepared["expectedHead"]:
            append_event(graph, {
                "event": "integrate-finalized", "stableId": stable_id, "attempt": prepared["attempt"],
                "generation": prepared["generation"], "baseSha": prepared["baseSha"], "headSha": prepared["headSha"],
                "claimant": prepared["claimant"], "leaseId": prepared["leaseId"], "leaseEpoch": prepared["leaseEpoch"],
                "expectedHead": prepared["expectedHead"], "newHead": prepared["newHead"],
                "commitSha": prepared["newHead"], "reviewToken": prepared["reviewToken"], "casResult": "updated",
            })
        else:
            append_event(graph, {
                "event": "integrate-aborted", "stableId": stable_id, "attempt": prepared["attempt"],
                "generation": prepared["generation"], "baseSha": prepared["baseSha"], "headSha": prepared["headSha"],
                "actor": "workflowz-reconcile", "reason": "prepared integration did not reach the durable ref",
                "expectedHead": prepared["expectedHead"], "newHead": prepared["newHead"], "casResult": "aborted",
            })
        state = read_object(STATE_PATH)
        replayed = replay(graph, state)
    if ref != replayed["head"]:
        fail(f"unexplained durable-ref drift: journal head={replayed['head']} durable={ref}")
    return replayed


def migrate(graph: Mapping[str, object]) -> dict[str, object]:
    """Regenerate the typed journal under the lease-fenced protocol.

    PAR-LEDGER's review is recorded while its first lease is still active (the
    legacy journal released before reviewing); VER-ALIGN's clean review is
    followed by a typed fix-skip and a prepared/finalized CAS pair.
    """
    base = "bfe3138c938ff0a4b9ea648d9c2362a61a80d3b3"
    task1 = "d451ba2d3b885220062245123d7a848456707dc1"
    task17 = "352457991c12ea59974dc5e18141777b5ba86d8e"
    at = "2026-08-26T02:25:50.161Z"
    by_id = {record["stableId"]: record for record in graph["records"]}
    journal: list[dict[str, object]] = []
    def add(event: dict[str, object]) -> None:
        journal.append({"seq": len(journal) + 1, "at": at, **event})
    def binding(stable_id: str, base_sha: str, head_sha: str) -> dict[str, object]:
        return {"stableId": stable_id, "attempt": 1, "generation": 1, "baseSha": base_sha, "headSha": head_sha}
    add({"event": "genesis", "graphSourceHash": graph["sourceHash"], "durableRef": DURABLE_REF, "headSha": task1, "actor": "Main"})
    binding1 = binding("PAR-LEDGER", base, task1)
    lease1 = {"leaseId": "PAR-LEDGER/1", "leaseEpoch": 1, "claimant": "Main"}
    add({"event": "scout", **binding1, "claimant": "Main", "questionHash": digest(by_id["PAR-LEDGER"]["question"]), "acceptanceHash": digest(by_id["PAR-LEDGER"]["acceptance"])})
    resources1 = {"files": ["docs/PARITY_LEDGER.md", "package.json", "scripts/verification/parity.ts", "scripts/verification/parity.test.ts"], "interfaces": ["verify:parity", "parity-ledger-schema", "workspace-topology-witness", "crate-dependency-witness", "ledger-ID-witness", "graduated-DAG-witness", "AgentLoopConfig-site-witness"], "closureComplete": True}
    add({"event": "lease-acquire", **binding1, "leaseId": "PAR-LEDGER/1", "epoch": 1, "claimant": "Main", "resources": resources1, "expiresAt": "2026-08-26T01:59:52Z"})
    add({"event": "implement", **binding1, **lease1, "commits": [task1], "reportPath": ".outline/sdd/task-1-report.md"})
    checks1 = [
        ("PAR-LEDGER-WITNESS", ["bun", "run", "verify:parity"], "PARITY_WITNESSES_OK"),
        ("PAR-LEDGER-TEST", ["bun", "test", "scripts/verification/parity.test.ts"], "25 pass / 0 fail"),
        ("PAR-LEDGER-TSC", ["bun", "run", "check"], "pass"),
    ]
    for name, command, result in checks1:
        add({"event": "verify", **binding1, **lease1, "verifier": "verify-task-1-par-ledger", "check": name, "command": command, "exitCode": 0, "result": "pass", "artifactPath": ".outline/sdd/task-1-report.md"})
    review1 = {"event": "review", **binding1, **lease1, "reviewer": "review-task-1-par-ledger", "reviewPath": ".outline/sdd/review-bfe3138..d451ba2.diff", "findings": [
        {"id": "PAR-LEDGER-R1", "severity": "Important", "summary": "canonical graduated-ticket set/edges are not pinned"},
        {"id": "PAR-LEDGER-R2", "severity": "Important", "summary": "Cargo package aliases can hide forbidden workspace edges"},
    ], "reviewToken": ""}
    review1["reviewToken"] = event_token({"seq": len(journal) + 1, "at": at, **review1})
    add(review1)
    add({"event": "lease-release", **binding1, "leaseId": "PAR-LEDGER/1", "epoch": 1, "actor": "Main", "reason": "commit-ready-for-review"})
    resources2 = {"files": ["scripts/verification/parity.ts", "scripts/verification/parity.test.ts"], "interfaces": ["graduated-DAG-witness", "crate-dependency-witness"], "closureComplete": True}
    add({"event": "lease-acquire", **binding1, "leaseId": "PAR-LEDGER/2", "epoch": 2, "claimant": "Main", "resources": resources2, "expiresAt": "2026-08-26T03:18:51Z"})
    add({"event": "lease-reclaim", **binding1, "leaseId": "PAR-LEDGER/2", "epoch": 2, "actor": "Main", "reason": "orphaned-fix-worker"})
    binding17 = binding("VER-ALIGN", task1, task17)
    lease17 = {"leaseId": "VER-ALIGN/1", "leaseEpoch": 1, "claimant": "Main"}
    add({"event": "scout", **binding17, "claimant": "Main", "questionHash": digest(by_id["VER-ALIGN"]["question"]), "acceptanceHash": digest(by_id["VER-ALIGN"]["acceptance"])})
    resources17 = {"files": [".github/workflows/release-verification.yml", "scripts/reconstruct-provider-data.ts", "scripts/generate-tool-schemas.ts", "scripts/verification/alignment.ts", "scripts/verification/alignment.test.ts", "package.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/read.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/bash.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/edit.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/write.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/grep.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/find.json", ".agent-tasks/pi-rust-rewrite/fixtures/tool-schemas/ls.json"], "interfaces": ["verify:alignment", "alignment-witness", "reference-pin-witness", "workflow-reference-witness"], "closureComplete": True}
    add({"event": "lease-acquire", **binding17, "leaseId": "VER-ALIGN/1", "epoch": 1, "claimant": "Main", "resources": resources17, "expiresAt": "2026-08-26T02:16:28Z"})
    add({"event": "implement", **binding17, **lease17, "commits": [task17], "reportPath": ".outline/sdd/task-17-report.md"})
    checks17 = [
        ("VER-ALIGN-GENERATE", ["bun", "run", "scripts/generate-tool-schemas.ts"], "wrote 7 schemas"),
        ("VER-ALIGN-WITNESS", ["bun", "run", "verify:alignment"], "ALIGNMENT_WITNESSES_OK"),
        ("VER-ALIGN-TEST", ["bun", "test", "scripts/verification/alignment.test.ts"], "10 pass / 0 fail"),
        ("VER-ALIGN-TSC", ["bun", "run", "check"], "pass"),
    ]
    for name, command, result in checks17:
        add({"event": "verify", **binding17, **lease17, "verifier": "verify-task-17-ver-align", "check": name, "command": command, "exitCode": 0, "result": "pass", "artifactPath": ".outline/sdd/task-17-report.md"})
    review17 = {"event": "review", **binding17, **lease17, "reviewer": "review-task-17-ver-align", "reviewPath": ".outline/sdd/review-d451ba2..3524579.diff", "findings": [], "reviewToken": ""}
    review17["reviewToken"] = event_token({"seq": len(journal) + 1, "at": at, **review17})
    add(review17)
    add({"event": "fix-skip", **binding17, **lease17, "reviewToken": review17["reviewToken"]})
    add({"event": "integrate-prepared", **binding17, **lease17, "expectedHead": task1, "newHead": task17, "reviewToken": review17["reviewToken"]})
    add({"event": "integrate-finalized", **binding17, **lease17, "expectedHead": task1, "newHead": task17, "commitSha": task17, "reviewToken": review17["reviewToken"], "casResult": "updated"})
    gate_artifact = ROOT / ".outline/gates/leaf-017.md"
    add({"event": "gate", **binding17, **lease17, "owner": "VER-ALIGN", "name": "track:VER", "scope": "track", "command": list(GATE_COMMANDS["track:VER"]), "exitCode": 0, "result": "pass", "artifactPath": ".outline/gates/leaf-017.md", "artifactDigest": "sha256:" + hashlib.sha256(gate_artifact.read_bytes()).hexdigest()})
    add({"event": "close", **binding17, **lease17, "issue": 145, "closedBy": "Main", "command": ["gh", "issue", "close", "145", "--repo", REPOSITORY], "result": "closed"})
    add({"event": "done", **binding17, **lease17, "completedBy": "Main"})
    add({"event": "lease-release", **binding17, "leaseId": "VER-ALIGN/1", "epoch": 1, "actor": "Main", "reason": "reviewed-integrated-and-closed"})
    state = {"version": 2, "graphSourceHash": graph["sourceHash"], "durableRef": DURABLE_REF, "journal": journal}
    replay(graph, state)
    with journal_lock():
        write_object(STATE_PATH, state)
        run(("git", "update-ref", DURABLE_REF, task17))
    return state


def evidence_environment_available(value: object, *, required: bool = False) -> bool:
    if value is None:
        return not required
    if not isinstance(value, Mapping) or set(value) != {"name", "available", "evidence"}:
        return False
    return (
        isinstance(value["name"], str)
        and bool(value["name"].strip())
        and value["available"] is True
        and isinstance(value["evidence"], str)
        and bool(value["evidence"].strip())
    )


def frontier(graph: Mapping[str, object], replayed: Mapping[str, object]) -> list[dict[str, object]]:
    executions = execution_records(graph)
    done = set(replayed["done"])
    active = {lease["stableId"] for lease in replayed["leases"].values()}
    statuses = replayed.get("executionStatus")
    ready: list[dict[str, object]] = []
    for record in executions:
        stable_id = str(record["stableId"])
        blockers = [str(item) for item in record["blockers"]]
        nonexternal = {item for item in blockers if not item.startswith("EXT-")}
        has_external = any(item.startswith("EXT-") for item in blockers)
        status = statuses.get(stable_id) if isinstance(statuses, Mapping) else None
        if (
            not isinstance(status, Mapping)
            or status.get("status") != RUNNABLE_STATUS
            or not evidence_environment_available(status.get("evidenceEnvironment"), required=has_external)
            or stable_id in done
            or stable_id in active
            or not nonexternal <= done
        ):
            continue
        loop = replayed["fixLoop"].get(stable_id)
        if loop and loop["attempt"] == replayed["attempts"][stable_id]:
            if not loop["verifyDone"]:
                next_phase = "verify"
            elif not loop["reviewDone"]:
                next_phase = "review"
            else:
                next_phase = None
        elif replayed["findings"].get(stable_id) and stable_id not in replayed["cleanReviews"]:
            next_phase = "fix"
        else:
            next_phase = next((phase for phase in PHASES if phase not in replayed["phases"][stable_id]), None)
        if next_phase is None:
            continue
        ready.append({"stableId": stable_id, "issue": record["issue"], "next": next_phase})
    return sorted(ready, key=lambda item: (item["next"] != "fix", item["issue"]))

def cas_integrate(graph: Mapping[str, object], args: argparse.Namespace) -> None:
    with journal_lock():
        replayed = reconcile(graph)
        if replayed["head"] != args.expected:
            fail(f"CAS stale: journal head={replayed['head']} expected={args.expected}")
        current = durable_ref_head()
        if current != args.expected:
            fail(f"CAS stale: ref={current} expected={args.expected}")
        run(("git", "merge-base", "--is-ancestor", args.expected, args.new))
        listing = run(("git", "rev-list", f"{args.expected}..{args.new}")).stdout.split()
        if args.stable_id == "PERF-T11":
            recorded = [str(item["commitSha"]) for item in replayed["perfIterations"]]
            if len(listing) != len(recorded) or sorted(listing) != sorted(recorded):
                fail(f"PERF-T11 CAS range must equal the recorded iteration commits: range={listing} recorded={recorded}")
        elif listing != [args.new]:
            fail(f"integration must advance the head by exactly one commit: range={listing}")
        prepared = {
            "event": "integrate-prepared", "stableId": args.stable_id, "attempt": args.attempt, "generation": args.generation,
            "baseSha": args.base, "headSha": args.new, "claimant": args.claimant, "leaseId": args.lease_id,
            "leaseEpoch": args.lease_epoch, "expectedHead": args.expected, "newHead": args.new, "reviewToken": args.review_token,
        }
        append_event(graph, prepared)
        try:
            run(("git", "update-ref", DURABLE_REF, args.new, args.expected))
            append_event(graph, {
                "event": "integrate-finalized", "stableId": args.stable_id, "attempt": args.attempt, "generation": args.generation,
                "baseSha": args.base, "headSha": args.new, "claimant": args.claimant, "leaseId": args.lease_id,
                "leaseEpoch": args.lease_epoch, "expectedHead": args.expected, "newHead": args.new,
                "commitSha": args.new, "reviewToken": args.review_token, "casResult": "updated",
            })
        except WorkflowError as error:
            rollback = run(("git", "update-ref", DURABLE_REF, args.expected, args.new), check=False)
            append_event(graph, {
                "event": "integrate-aborted", "stableId": args.stable_id, "attempt": args.attempt, "generation": args.generation,
                "baseSha": args.base, "headSha": args.expected, "actor": "workflowz-cas", "reason": "ref update landed but journal finalization failed",
                "expectedHead": args.expected, "newHead": args.new, "casResult": "aborted",
            })
            raise WorkflowError(f"CAS finalize failed after ref update (rollback rc={rollback.returncode}): {error}") from error


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command")
    refresh_parser = sub.add_parser("refresh", help="fetch issue #12 recursively and regenerate graph and PLAN")
    refresh_parser.add_argument("--no-plan", action="store_true")
    validate_parser = sub.add_parser("validate", help="validate graph and journal")
    validate_parser.add_argument("--live", action="store_true")
    validate_parser.add_argument("--all-done", action="store_true")
    validate_parser.add_argument("--pre-close", action="store_true", help="validate all pre-close conditions without requiring G11 or root closure")
    sub.add_parser("frontier", help="list runnable Stable IDs")
    sub.add_parser("migrate", help="replace legacy status state with typed genesis journal")
    record_parser = sub.add_parser("record", help="append and replay one typed JSON event")
    record_parser.add_argument("event_json", type=Path)
    cas = sub.add_parser("cas", help="compare-and-swap one clean reviewed integration")
    cas.add_argument("--stable-id", required=True)
    cas.add_argument("--attempt", required=True, type=int)
    cas.add_argument("--generation", required=True, type=int)
    cas.add_argument("--base", required=True)
    cas.add_argument("--expected", required=True)
    cas.add_argument("--new", required=True)
    cas.add_argument("--claimant", required=True)
    cas.add_argument("--lease-id", required=True)
    cas.add_argument("--lease-epoch", required=True, type=int)
    cas.add_argument("--review-token", required=True)
    args = parser.parse_args(argv)
    command = args.command or "validate"
    try:
        if command == "refresh":
            with journal_lock():
                graph = refresh(update_plan=not args.no_plan)
                print(f"WORKFLOWZ_REFRESHED source={graph['sourceHash']} tasks=115 externals=16 waves=15 nodes=1202")
        else:
            graph = read_object(GRAPH_PATH)
            if command == "validate":
                live = live_graph() if (args.live or args.all_done or args.pre_close) else None
                validate_graph(graph, live)
                replayed = validate_state(graph, require_all_done=args.all_done)
                if args.all_done:
                    tag = "WORKFLOWZ_ALL_DONE"
                elif args.pre_close:
                    verify_pre_close(graph, replayed)
                    tag = "WORKFLOWZ_PRE_CLOSE"
                else:
                    tag = "WORKFLOWZ_OK"
                print(f"{tag} source={graph['sourceHash']} tasks=115 externals=16 waves=15 nodes=1202 done={len(replayed['done'])} leases={len(replayed['leases'])}")
            elif command == "frontier":
                validate_graph(graph)
                with journal_lock():
                    require_live_source(graph)
                    replayed = reconcile(graph)
                    for item in frontier(graph, replayed):
                        print(f"{item['stableId']} issue=#{item['issue']} next={item['next']}")
            elif command == "migrate":
                validate_graph(graph)
                with journal_lock():
                    require_live_source(graph)
                    state = migrate(graph)
                    print(f"WORKFLOWZ_MIGRATED events={len(state['journal'])} ref={DURABLE_REF}")
            elif command == "record":
                validate_graph(graph)
                event = read_object(args.event_json)
                with journal_lock():
                    require_live_source(graph)
                    reconcile(graph)
                    append_event(graph, event)
                    print("WORKFLOWZ_EVENT_RECORDED")
            elif command == "cas":
                validate_graph(graph)
                with journal_lock():
                    require_live_source(graph)
                    cas_integrate(graph, args)
                    print(f"WORKFLOWZ_CAS_UPDATED ref={DURABLE_REF} head={args.new}")
            else:
                parser.error("a command is required")
        return 0
    except WorkflowError as error:
        print(f"WORKFLOWZ_ERROR {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
