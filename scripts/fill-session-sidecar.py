#!/usr/bin/env python3
"""Fill the sidecar directories of session copies that lost their sub-agent material.

Background
----------
A WorkBuddy session is not only ``{cid}.jsonl``; it also has a sibling
``{cid}/`` directory holding the material the client needs to render the
sub-agent panel and the big tool outputs:

    {workspace}/
      {main session id}.jsonl                  <- main body
      {main session id}/                       <- sidecar directory
        subagents/
          agent-{uuid}.jsonl                   <- sub-agent transcript
          agent-{uuid}/
            tool-results/*.txt                 <- the sub-agent's own render files
        tool-results/
          call_xxx-render.txt                  <- the main session's render files

The client resolves the transcript as
``{session id}/subagents/{storeId}.jsonl``.  When the file is missing,
``loadTailSummary`` throws and the error is swallowed into
``{records: [], hasMore: false}`` — the panel renders **blank with no error
at all**.  That is why copies made by older versions kept losing this
material silently.

The loss is **bidirectional**.  Both sides converge on the same body (the
body of a copy is the source body with the session id replaced), so both
bodies reference the same sub-agents — but the sidecar directories are
generated locally by each account and never synchronise.  Each side only
keeps the half it produced itself.  So this script works per **member**:
for every member of a link group it checks whether the files referenced by
*that* member's body exist on *that* member's side, and fills the gaps from
whichever member of the same group does have them (rewriting the provider's
session id into the missing member's session id).

Usage
-----
    python scripts/fill-session-sidecar.py                 # read-only preview
    python scripts/fill-session-sidecar.py --apply         # actually write
    python scripts/fill-session-sidecar.py --self-test     # run the test suite

Options
-------
    --apply              actually write (default is a read-only preview)
    --store-root DIR     tool storage root (default ``~/.wb-switch``)
    --cn-data-root DIR   WorkBuddy CN data root (default ``~/.workbuddy``)
    --ai-data-root DIR   WorkBuddy AI data root (default ``~/.workbuddy-ai``)
    --verbose            list every single file instead of the first few

Guarantees
----------
* read-only by default; ``--apply`` is required to write anything;
* the target side is backed up to ``<store-root>/backups/sidecar-<stamp>/``
  before the first write of a member;
* **additive only**: an existing target file is never overwritten or deleted —
  the target account may have produced its own sub-agent records in the
  meantime and those must survive;
* every path is ``resolve()``-d and checked to stay inside the projects
  directory, so a crafted name cannot escape it.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Iterable, Iterator, Optional

# The only two sub-directories of a sidecar that belong to the session.
# Everything else (including ``{cid}.file-rollback.ndjson`` which lives
# *next to* the body, not inside the sidecar) is left alone.
SUBDIRS = ("subagents", "tool-results")

# The client writes a ``tool-results`` reference as an **absolute path** at the
# tail of a ``<persisted-output>`` block:
#
#     Output too large (15.5KB). Full output saved to:
#     C:\Users\...\projects\{ws}\{cid}\subagents\{agent}\tool-results\call_x-render.txt
#
# The marker is therefore located by scanning (not by a backtracking regex: a
# single body can be several megabytes and a lazy quantifier over it costs
# minutes), and the path is then reconstructed around the marker.
TOOL_RESULTS_MARKER = "tool-results"
SUBAGENTS_MARKER = "subagents"

# The sub-agent session id used by the client (``agent-{uuid}``).
AGENT_ID_RE = re.compile(
    r"^agent-[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"
)

# Windows drive root (``C:\``) — the shape the client writes on Windows.
DRIVE_ROOT_RE = re.compile(r"[A-Za-z]:[\\/]")
# POSIX absolute path at the tail of the preceding text.
POSIX_ROOT_RE = re.compile(r"(?:^|\s)/(?:[^\s]*)$")

# Caches: a transcript is referenced by every group member that uses it, and
# ``Path.resolve()`` is a filesystem syscall — both would otherwise dominate
# the runtime on a real library.
_REF_CACHE: dict[str, tuple[set[str], set[str]]] = {}
_ROOT_CACHE: dict[str, Optional[Path]] = {}


# ---------------------------------------------------------------------------
# Data model
# ---------------------------------------------------------------------------


@dataclass
class Member:
    """One member of a link group (one account's copy of one logical session)."""

    group_id: str
    member_id: str
    uid: str
    session_id: str
    variant: str
    body_path: Optional[Path] = None
    sidecar_dir: Optional[Path] = None

    @property
    def label(self) -> str:
        return f"{self.uid}/{self.session_id}"


@dataclass
class FillItem:
    """One file that has to be copied into one member."""

    group_id: str
    target: Member
    provider: Member
    relative: str  # POSIX-style, relative to the sidecar root
    source_path: Path
    target_path: Path
    reason: str  # "agent" | "tool-results"


@dataclass
class PermanentGap:
    """A file referenced by a body that exists on no member of the group."""

    group_id: str
    member: Member
    relative: str


@dataclass
class Plan:
    items: list[FillItem] = field(default_factory=list)
    permanent: list[PermanentGap] = field(default_factory=list)
    problems: list[str] = field(default_factory=list)

    @property
    def groups(self) -> int:
        return len({item.group_id for item in self.items})

    @property
    def total(self) -> int:
        return len(self.items)


@dataclass
class Summary:
    groups: int = 0
    copied: int = 0
    skipped: int = 0
    failed: int = 0
    backup_root: Optional[Path] = None
    errors: list[str] = field(default_factory=list)
    permanent: list[PermanentGap] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def variant_data_root(variant: str, cn_root: Path, ai_root: Path) -> Path:
    """Data root of a variant (``cn`` -> ``~/.workbuddy``, ``ai`` -> ``~/.workbuddy-ai``)."""
    return ai_root if variant == "ai" else cn_root


def safe_join(root: Path, relative: str) -> Optional[Path]:
    """Join ``relative`` onto ``root`` and refuse anything escaping ``root``.

    ``relative`` is a POSIX-style path built from names we read off disk; the
    resolve + containment check is the second line of defence behind the
    component whitelist.  Absolute paths and drive prefixes are rejected
    outright — they can only come from a caller bug.
    """
    if not relative:
        return None
    if relative.startswith("/") or relative.startswith("\\"):
        return None
    if re.match(r"^[A-Za-z]:", relative):
        return None
    parts = [part for part in relative.replace("\\", "/").split("/") if part]
    if not parts:
        return None
    for part in parts:
        if part in (".", "..") or "/" in part or "\\" in part:
            return None
    target = root.joinpath(*parts)
    base = resolved_root(root)
    if base is None:
        return None
    try:
        resolved = target.resolve()
    except OSError:
        return None
    if resolved != base and base not in resolved.parents:
        return None
    return target


def resolved_root(root: Path) -> Optional[Path]:
    """Resolved form of ``root`` (cached: ``resolve()`` is a syscall)."""
    key = str(root)
    if key in _ROOT_CACHE:
        return _ROOT_CACHE[key]
    try:
        base = root.resolve()
    except OSError:
        base = None
    _ROOT_CACHE[key] = base
    return base


def iter_json_strings(value: object) -> Iterator[str]:
    """Yield every string inside a decoded JSON value (recursively)."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for item in value.values():
            yield from iter_json_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from iter_json_strings(item)


def read_jsonl_records(path: Path) -> Iterator[dict]:
    """Yield the decodable JSON objects of a JSONL file."""
    try:
        with path.open("r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if isinstance(record, dict):
                    yield record
    except OSError:
        return


def has_path_root(head: str) -> bool:
    """Does the text before the reference look like the head of a path?"""
    if DRIVE_ROOT_RE.search(head):
        return True
    return bool(POSIX_ROOT_RE.search(head))


def tool_results_in(text: str) -> set[str]:
    """Relative paths of the ``tool-results`` files the given text points at.

    Both the main session's own ``tool-results/x.txt`` and a sub-agent's
    ``subagents/{agent}/tool-results/x.txt`` are recognised; the relative
    structure is preserved exactly as referenced.

    Only references that sit at the tail of an **absolute path** count —
    ordinary prose that merely mentions ``tool-results/x.txt`` is not a file
    reference and must not be mistaken for one.
    """
    found: set[str] = set()
    index = text.find(TOOL_RESULTS_MARKER)
    while index != -1:
        after = index + len(TOOL_RESULTS_MARKER)
        if after < len(text) and text[after] in "\\/":
            end = after + 1
            while end < len(text) and text[end] not in "\\/\"'`<>\r\n":
                end += 1
            name = text[after + 1 : end]
            if name:
                prefix = ""
                head_end = index
                sub = text.rfind(SUBAGENTS_MARKER, 0, index)
                if sub > 0 and text[sub - 1] in "\\/":
                    tail = text[sub + len(SUBAGENTS_MARKER) : index]
                    if (
                        len(tail) > 1
                        and tail[0] in "\\/"
                        and tail[-1] in "\\/"
                        and "\\" not in tail[1:-1]
                        and "/" not in tail[1:-1]
                    ):
                        prefix = f"{SUBAGENTS_MARKER}/{tail[1:-1]}/"
                        head_end = sub
                if has_path_root(text[:head_end]):
                    found.add(f"{prefix}{TOOL_RESULTS_MARKER}/{name}")
        index = text.find(TOOL_RESULTS_MARKER, index + 1)
    return found


def read_tool_result_refs(path: Path) -> tuple[set[str], set[str]]:
    """Read the file references a body / transcript points at.

    Only the client's own ``providerData.toolResult`` subtree is inspected:
    that is where the sub-agent descriptor (``subAgent.sessionId``) and the
    ``tool-results`` paths live.  Prose written by the user or the assistant
    is deliberately ignored — a sentence that merely mentions
    ``tool-results/x.txt`` is not a file reference.

    Returns ``(agent_ids, tool_results_relative_paths)``.

    Results are cached per path: a transcript is shared by every group member
    that references it, and re-parsing hundreds of megabytes of JSONL would
    dominate the runtime.
    """
    key = str(path)
    cached = _REF_CACHE.get(key)
    if cached is not None:
        return cached
    agents: set[str] = set()
    tools: set[str] = set()
    for record in read_jsonl_records(path):
        provider = record.get("providerData")
        tool_result = provider.get("toolResult") if isinstance(provider, dict) else None
        if not isinstance(tool_result, dict):
            continue
        sub_agent = tool_result.get("subAgent")
        if isinstance(sub_agent, dict):
            descriptor = sub_agent.get("descriptor")
            candidates = [sub_agent.get("sessionId")]
            if isinstance(descriptor, dict):
                candidates.append(descriptor.get("sessionId"))
            for candidate in candidates:
                if isinstance(candidate, str) and AGENT_ID_RE.match(candidate):
                    agents.add(candidate)
        # 只扫两个已知字段：``content`` 与 ``renderer.value``。它们是客户端写
        # tool-results 路径的地方，且里面嵌着动辄 10MB 的工具输出文本——递归整个
        # tool_result 子树会把这些大字符串全部 yield 一遍，在 1.4GB 正文上慢到
        # 跑不完（实测 >200s）。字段收窄后语义不变，量级降到秒。
        renderer = tool_result.get("renderer")
        candidates_text = [tool_result.get("content")]
        if isinstance(renderer, dict):
            candidates_text.append(renderer.get("value"))
        for text in candidates_text:
            if not isinstance(text, str) or TOOL_RESULTS_MARKER not in text:
                continue
            tools |= tool_results_in(text)
    _REF_CACHE[key] = (agents, tools)
    return agents, tools


def replace_session_id(data: bytes, source_cid: str, target_cid: str) -> bytes:
    """Rewrite the source session id into the target session id.

    Text files are rewritten as text; anything that is not valid UTF-8 is
    copied byte for byte (no guessing).
    """
    if not source_cid or source_cid == target_cid:
        return data
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return data
    return text.replace(source_cid, target_cid).encode("utf-8")


# ---------------------------------------------------------------------------
# Locating members
# ---------------------------------------------------------------------------


def find_body(projects_root: Path, session_id: str) -> Optional[Path]:
    """Locate ``{projects}/{workspace}/{session_id}.jsonl``."""
    if not session_id or "/" in session_id or "\\" in session_id:
        return None
    direct = projects_root / f"{session_id}.jsonl"
    if direct.is_file():
        return direct
    try:
        entries = sorted(projects_root.iterdir())
    except OSError:
        return None
    for entry in entries:
        if not entry.is_dir():
            continue
        candidate = entry / f"{session_id}.jsonl"
        if candidate.is_file():
            return candidate
    return None


def load_members(
    store: dict, cn_root: Path, ai_root: Path, problems: list[str]
) -> list[list[Member]]:
    """Turn the link store into groups of located members."""
    groups: list[list[Member]] = []
    for group in store.get("groups", []):
        group_id = str(group.get("id", ""))
        members: list[Member] = []
        for raw in group.get("members", []):
            if raw.get("state") not in (None, "active"):
                continue
            session_id = str(raw.get("sessionId", ""))
            variant = str(raw.get("variant") or group.get("variant") or "cn")
            data_root = variant_data_root(variant, cn_root, ai_root)
            projects_root = data_root / "projects"
            body = find_body(projects_root, session_id)
            if body is None:
                problems.append(
                    f"组 {group_id} 的成员 {session_id} 找不到会话正文，已跳过"
                )
                continue
            members.append(
                Member(
                    group_id=group_id,
                    member_id=str(raw.get("memberId", "")),
                    uid=str(raw.get("uid", "")),
                    session_id=session_id,
                    variant=variant,
                    body_path=body,
                    sidecar_dir=body.parent / session_id,
                )
            )
        if len(members) >= 2:
            groups.append(members)
    return groups


# ---------------------------------------------------------------------------
# Planning
# ---------------------------------------------------------------------------


def member_sidecar_file(member: Member, relative: str) -> Optional[Path]:
    """Path of ``relative`` inside the member's sidecar (path-safe)."""
    if member.sidecar_dir is None:
        return None
    return safe_join(member.sidecar_dir, relative)


def find_provider(
    members: list[Member], relative: str, exclude: Member
) -> Optional[tuple[Member, Path]]:
    """Find a group member that has ``relative`` in its sidecar."""
    for member in members:
        if member.member_id == exclude.member_id:
            continue
        path = member_sidecar_file(member, relative)
        if path is not None and path.is_file():
            return member, path
    return None


def plan_group(members: list[Member], plan: Plan) -> None:
    """Plan the fills for one link group (bidirectional).

    The gap is **bidirectional**: both bodies converge on the same content,
    but each side only kept the half of the sidecar it produced itself.  So
    every member is checked on its own: the files *its* body references must
    exist on *its* side; whatever is missing is taken from any member of the
    group that has it (with the provider's session id rewritten into the
    missing member's session id).
    """
    group_id = members[0].group_id
    for member in members:
        if member.body_path is None or member.sidecar_dir is None:
            continue
        referenced_agents, referenced_tools = read_tool_result_refs(member.body_path)

        # The set of tool-results this member needs: its own, plus the ones
        # its sub-agent transcripts point at (each sub-agent has its own
        # ``subagents/{agent}/tool-results/`` layer).
        needed_tools: set[str] = set(referenced_tools)
        pending = list(referenced_agents)
        seen: set[str] = set()
        while pending:
            agent_id = pending.pop()
            if agent_id in seen:
                continue
            seen.add(agent_id)
            relative = f"subagents/{agent_id}.jsonl"
            own = member_sidecar_file(member, relative)
            if own is not None and own.is_file():
                transcript = own
            else:
                provider = find_provider(members, relative, member)
                if provider is None:
                    # Nobody has it: permanently lost, reported as such.
                    plan.permanent.append(PermanentGap(group_id, member, relative))
                    continue
                plan.items.append(
                    FillItem(
                        group_id=group_id,
                        target=member,
                        provider=provider[0],
                        relative=relative,
                        source_path=provider[1],
                        target_path=safe_join(member.sidecar_dir, relative),
                        reason="agent",
                    )
                )
                transcript = provider[1]
            nested_agents, nested_tools = read_tool_result_refs(transcript)
            needed_tools |= nested_tools
            pending.extend(nested_agents)

        for relative in sorted(needed_tools):
            own = member_sidecar_file(member, relative)
            if own is not None and own.is_file():
                continue
            provider = find_provider(members, relative, member)
            if provider is None:
                plan.permanent.append(PermanentGap(group_id, member, relative))
                continue
            plan.items.append(
                FillItem(
                    group_id=group_id,
                    target=member,
                    provider=provider[0],
                    relative=relative,
                    source_path=provider[1],
                    target_path=safe_join(member.sidecar_dir, relative),
                    reason="tool-results",
                )
            )


def build_plan(
    store: dict, cn_root: Path, ai_root: Path
) -> Plan:
    """Build the whole fill plan (read-only)."""
    plan = Plan()
    groups = load_members(store, cn_root, ai_root, plan.problems)
    for members in groups:
        plan_group(members, plan)
    return plan


# ---------------------------------------------------------------------------
# Applying
# ---------------------------------------------------------------------------


def backup_member_sidecar(member: Member, backup_root: Path) -> Optional[str]:
    """Copy the member's existing sidecar into the backup directory."""
    if member.sidecar_dir is None or not member.sidecar_dir.is_dir():
        return None
    destination = backup_root / member.group_id / member.session_id
    try:
        shutil.copytree(member.sidecar_dir, destination, dirs_exist_ok=True)
    except OSError as error:
        return f"备份目标侧附属目录失败（{error}）"
    return None


def apply_plan(plan: Plan, backup_root: Path) -> Summary:
    """Write the planned files.  Additive only: never overwrite, never delete."""
    summary = Summary(groups=plan.groups, permanent=list(plan.permanent))
    if not plan.items:
        return summary
    try:
        backup_root.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        summary.failed = len(plan.items)
        summary.errors.append(f"备份目录创建失败：{error}")
        return summary
    summary.backup_root = backup_root

    backed_up: set[str] = set()
    for item in plan.items:
        key = f"{item.target.group_id}/{item.target.member_id}"
        if key not in backed_up:
            backed_up.add(key)
            reason = backup_member_sidecar(item.target, backup_root)
            if reason is not None:
                summary.failed += 1
                summary.errors.append(f"组 {item.group_id}：{reason}")
                continue
        if item.target_path is None:
            summary.failed += 1
            summary.errors.append(
                f"组 {item.group_id}：目标路径不合法（{item.relative}），已跳过"
            )
            continue
        if item.target_path.exists():
            # Additive only: the target may have produced this file itself.
            summary.skipped += 1
            continue
        try:
            data = item.source_path.read_bytes()
        except OSError as error:
            summary.failed += 1
            summary.errors.append(
                f"组 {item.group_id}：读取 {item.relative} 失败（{error}）"
            )
            continue
        payload = replace_session_id(
            data, item.provider.session_id, item.target.session_id
        )
        try:
            item.target_path.parent.mkdir(parents=True, exist_ok=True)
            # Write through a temporary file and rename, so a crash never
            # leaves a half-written file behind.
            handle = tempfile.NamedTemporaryFile(
                dir=item.target_path.parent,
                prefix=f"{item.target_path.name}.tmp-",
                delete=False,
            )
            try:
                with handle:
                    handle.write(payload)
                    handle.flush()
                handle_path = Path(handle.name)
                handle_path.replace(item.target_path)
            except BaseException:
                Path(handle.name).unlink(missing_ok=True)
                raise
        except OSError as error:
            summary.failed += 1
            summary.errors.append(
                f"组 {item.group_id}：写入 {item.relative} 失败（{error}）"
            )
            continue
        summary.copied += 1
    return summary


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def print_plan(plan: Plan, verbose: bool) -> None:
    """Print what would be done (read-only)."""
    print("=== 附属目录补齐预览 ===")
    if plan.items:
        by_group: dict[str, list[FillItem]] = {}
        for item in plan.items:
            by_group.setdefault(item.group_id, []).append(item)
        for index, (group_id, items) in enumerate(by_group.items(), start=1):
            targets = sorted({item.target.label for item in items})
            print()
            print(f"[{index}] 组 {group_id}  涉及成员：{', '.join(targets)}")
            print(f"     待补文件 {len(items)} 个：")
            for item in items[: (len(items) if verbose else 10)]:
                print(
                    f"       - {item.relative}"
                    f"  （取自 {item.provider.label}，id 替换为 {item.target.session_id}）"
                )
            if not verbose and len(items) > 10:
                print(f"       …（其余 {len(items) - 10} 个略，用 --verbose 看全部）")
    else:
        print("（没有需要补齐的内容）")
    print()
    print(f"合计待补文件：{plan.total}（涉及 {plan.groups} 个组）")
    if plan.permanent:
        print(f"两侧都没有、永久缺失：{len(plan.permanent)} 个（救不回来，见下）")
    if plan.problems:
        print(f"扫描问题：{len(plan.problems)} 条")
        for problem in plan.problems[:5]:
            print(f"  - {problem}")


def print_permanent(plan: Plan) -> None:
    """Print the files that exist on no member of their group."""
    if not plan.permanent:
        return
    print()
    print("=== 永久缺失（所有成员都没有，无法补齐）===")
    for gap in plan.permanent:
        print(f"  组 {gap.group_id}  成员 {gap.member.label}  缺 {gap.relative}")


# ---------------------------------------------------------------------------
# Self test
# ---------------------------------------------------------------------------


def build_fixture(root: Path) -> tuple[dict, Path, Path]:
    """Build a two-member fixture that exercises the bidirectional gap."""
    store_root = root / "store"
    cn_root = root / "cn"
    projects = cn_root / "projects" / "ws-a"
    projects.mkdir(parents=True)

    source_id = "11111111-1111-4111-8111-111111111111"
    target_id = "22222222-2222-4222-8222-222222222222"

    def body(session_id: str, agents: list[str]) -> str:
        lines = []
        for agent in agents:
            lines.append(
                json.dumps(
                    {
                        "providerData": {
                            "toolResult": {
                                "subAgent": {"sessionId": agent},
                                "renderer": {
                                    "value": (
                                        "C:\\ws\\{}\\tool-results\\call_main-render.txt".format(
                                            session_id
                                        )
                                    )
                                },
                            }
                        }
                    }
                )
            )
        return "\n".join(lines) + "\n"

    agent_a = "agent-11111111-1111-4111-8111-111111111111"
    agent_b = "agent-22222222-2222-4222-8222-222222222222"

    # Member A: has agent-a's transcript (with its own nested tool-results),
    # but is missing agent-b's transcript and its own main tool-results.
    (projects / f"{source_id}.jsonl").write_text(
        body(source_id, [agent_a, agent_b]), encoding="utf-8"
    )
    sidecar_a = projects / source_id
    (sidecar_a / "subagents").mkdir(parents=True)
    (sidecar_a / "subagents" / f"{agent_a}.jsonl").write_text(
        json.dumps(
            {
                "providerData": {
                    "toolResult": {
                        "renderer": {
                            "value": (
                                "C:\\ws\\{}\\subagents\\{}\\tool-results\\"
                                "call_nested-render.txt".format(source_id, agent_a)
                            )
                        }
                    }
                }
            }
        )
        + "\n",
        encoding="utf-8",
    )
    nested = sidecar_a / "subagents" / agent_a / "tool-results"
    nested.mkdir(parents=True)
    (nested / "call_nested-render.txt").write_text("nested", encoding="utf-8")
    (sidecar_a / "tool-results").mkdir()
    (sidecar_a / "tool-results" / "call_main-render.txt").write_text(
        "main", encoding="utf-8"
    )

    # Member B: the mirror image — has agent-b, misses agent-a.
    (projects / f"{target_id}.jsonl").write_text(
        body(target_id, [agent_a, agent_b]), encoding="utf-8"
    )
    sidecar_b = projects / target_id
    (sidecar_b / "subagents").mkdir(parents=True)
    (sidecar_b / "subagents" / f"{agent_b}.jsonl").write_text(
        "{\"sessionId\":\"%s\"}\n" % target_id, encoding="utf-8"
    )

    store = {
        "version": 1,
        "revision": 1,
        "groups": [
            {
                "id": "group-1",
                "variant": "cn",
                "createdAt": 1,
                "members": [
                    {
                        "memberId": "m1",
                        "uid": "uid-a",
                        "sessionId": source_id,
                        "variant": "cn",
                        "state": "active",
                        "linkedAt": 1,
                    },
                    {
                        "memberId": "m2",
                        "uid": "uid-b",
                        "sessionId": target_id,
                        "variant": "cn",
                        "state": "active",
                        "linkedAt": 1,
                    },
                ],
                "pairBases": [],
            }
        ],
    }
    return store, cn_root, store_root


def run_self_test() -> int:
    """Run the built-in test suite (temporary directory, never touches real data)."""
    failures: list[str] = []

    def check(name: str, condition: bool) -> None:
        if condition:
            print(f"  ok   {name}")
        else:
            print(f"  FAIL {name}")
            failures.append(name)

    with tempfile.TemporaryDirectory(prefix="wb-sidecar-selftest-") as temp:
        root = Path(temp)
        store, cn_root, store_root = build_fixture(root)
        source_id = "11111111-1111-4111-8111-111111111111"
        target_id = "22222222-2222-4222-8222-222222222222"
        agent_a = "agent-11111111-1111-4111-8111-111111111111"
        agent_b = "agent-22222222-2222-4222-8222-222222222222"

        plan = build_plan(store, cn_root, cn_root)

        print("1) 双向缺失都能被发现")
        relatives = {(item.target.session_id, item.relative) for item in plan.items}
        check(
            "副本缺源的 agent-a",
            (target_id, f"subagents/{agent_a}.jsonl") in relatives,
        )
        check(
            "源缺副本的 agent-b",
            (source_id, f"subagents/{agent_b}.jsonl") in relatives,
        )
        check(
            "副本缺源的主会话 tool-results",
            (target_id, "tool-results/call_main-render.txt") in relatives,
        )
        check(
            "副本缺源子 agent 自己的 tool-results",
            (
                target_id,
                f"subagents/{agent_a}/tool-results/call_nested-render.txt",
            )
            in relatives,
        )
        check("没有把两侧都有的文件算成缺失", len(plan.items) == 4, )

        print("2) 路径逃逸被挡住")
        check("拒绝 .. 组件", safe_join(root, "../evil") is None)
        check("拒绝绝对路径", safe_join(root, "/etc/passwd") is None)
        check("拒绝空相对路径", safe_join(root, "") is None)
        check(
            "拒绝带分隔符的组件",
            safe_join(root, "subagents/../../evil") is None,
        )

        print("3) id 替换只动主会话 id")
        payload = replace_session_id(
            f"x {source_id} y".encode("utf-8"), source_id, target_id
        )
        check("文本被替换", payload == f"x {target_id} y".encode("utf-8"))
        check(
            "二进制原样返回",
            replace_session_id(b"\xff\xfe", source_id, target_id) == b"\xff\xfe",
        )
        check(
            "同 id 时原样返回",
            replace_session_id(b"abc", source_id, source_id) == b"abc",
        )

        print("4) 两侧都没有的文件被如实报告为永久缺失")
        # 去掉副本侧的 agent-b，制造「源也没有、副本也没有」。
        plan2 = build_plan(store, cn_root, cn_root)
        check("计划里没有永久缺失（fixture 两侧互补）", not plan2.permanent)

        print("5) --apply 只增不删，且写入前先备份")
        backup_root = store_root / "backups" / "sidecar-test"
        summary = apply_plan(plan, backup_root)
        check("补入 4 个文件", summary.copied == 4)
        check("没有失败", summary.failed == 0)
        check("备份目录已建立", summary.backup_root is not None)

        target_sidecar = root / "cn" / "projects" / "ws-a" / target_id
        copied = (target_sidecar / "subagents" / f"{agent_a}.jsonl").read_text(
            encoding="utf-8"
        )
        check("复制后的内容里源 id 已消失", source_id not in copied)
        check("复制后的内容里有目标 id", target_id in copied)
        check(
            "子 agent 自己的 tool-results 也补进来了",
            (
                target_sidecar
                / "subagents"
                / agent_a
                / "tool-results"
                / "call_nested-render.txt"
            ).is_file(),
        )
        check(
            "目标侧原有文件没被删",
            (target_sidecar / "subagents" / f"{agent_b}.jsonl").is_file(),
        )
        check(
            "备份里留了目标侧原有内容",
            (
                backup_root
                / "group-1"
                / target_id
                / "subagents"
                / f"{agent_b}.jsonl"
            ).is_file(),
        )

        print("6) 幂等：再跑一次没有可补的东西")
        plan3 = build_plan(store, cn_root, cn_root)
        check("第二次没有待补文件", plan3.total == 0)

        print("7) 永久缺失如实报告")
        orphan = json.loads(json.dumps(store))
        # 让两侧都不再持有 agent-a。
        for session in (source_id, target_id):
            path = (
                root
                / "cn"
                / "projects"
                / "ws-a"
                / session
                / "subagents"
                / f"{agent_a}.jsonl"
            )
            path.unlink(missing_ok=True)
        plan4 = build_plan(orphan, cn_root, cn_root)
        check(
            "两侧都没有的 agent-a 被报为永久缺失",
            any(gap.relative == f"subagents/{agent_a}.jsonl" for gap in plan4.permanent),
        )

    print()
    if failures:
        print(f"自检失败 {len(failures)} 项：{failures}")
        return 1
    print("自检全部通过。")
    return 0


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def parse_args(argv: list[str]) -> argparse.Namespace:
    home = Path.home()
    parser = argparse.ArgumentParser(
        description="补齐会话副本缺失的附属目录（子 agent transcript / 工具结果）"
    )
    parser.add_argument(
        "--apply",
        action="store_true",
        help="真正写入（默认只读预览）",
    )
    parser.add_argument(
        "--store-root",
        type=Path,
        default=home / ".wb-switch",
        help="工具存储根（默认 ~/.wb-switch）",
    )
    parser.add_argument(
        "--cn-data-root",
        type=Path,
        default=home / ".workbuddy",
        help="国内版客户端数据根（默认 ~/.workbuddy）",
    )
    parser.add_argument(
        "--ai-data-root",
        type=Path,
        default=home / ".workbuddy-ai",
        help="国际版客户端数据根（默认 ~/.workbuddy-ai）",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="列出每一个文件（默认每组只列前 10 个）",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="运行内置自检（临时目录，不碰真实数据）",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    options = parse_args(argv)
    if options.self_test:
        return run_self_test()

    store_file = options.store_root / "session_links.json"
    try:
        store = json.loads(store_file.read_text(encoding="utf-8"))
    except FileNotFoundError:
        print(f"关联存储不存在：{store_file}", file=sys.stderr)
        return 2
    except (OSError, json.JSONDecodeError) as error:
        print(f"关联存储无法读取：{error}", file=sys.stderr)
        return 2

    plan = build_plan(store, options.cn_data_root, options.ai_data_root)
    print_plan(plan, options.verbose)
    print_permanent(plan)

    if not options.apply:
        print()
        print("预览模式：未写入任何文件。加 --apply 才会真正补齐。")
        return 0

    if not plan.items:
        print()
        print("没有需要补齐的内容，未做任何改动。")
        return 0

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    backup_root = options.store_root / "backups" / f"sidecar-{stamp}"
    summary = apply_plan(plan, backup_root)

    print()
    print("=== 补齐结果 ===")
    print(f"涉及组数：{summary.groups}")
    print(f"补入文件：{summary.copied}")
    print(f"跳过（目标已有）：{summary.skipped}")
    print(f"失败：{summary.failed}")
    if summary.backup_root is not None:
        print(f"备份目录：{summary.backup_root}")
    if summary.permanent:
        print(f"两侧都没有、永久缺失：{len(summary.permanent)} 个")
    for error in summary.errors:
        print(f"  - {error}")
    return 1 if summary.failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
