#!/usr/bin/env python3
"""Render per-module core/orchestration coverage from an LCOV report."""

from __future__ import annotations

import argparse
import sys
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class ModuleCoverage:
    lines: dict[int, int] = field(default_factory=dict)
    functions: dict[str, int] = field(default_factory=dict)

    @property
    def covered_lines(self) -> int:
        return sum(hit > 0 for hit in self.lines.values())

    @property
    def total_lines(self) -> int:
        return len(self.lines)

    @property
    def covered_functions(self) -> int:
        return sum(hit > 0 for hit in self.functions.values())

    @property
    def total_functions(self) -> int:
        return len(self.functions)


def repository_path(source: str) -> str:
    normalized = source.replace("\\", "/")
    marker = "/src/"
    if marker in normalized:
        return "src/" + normalized.rsplit(marker, 1)[1]
    return normalized.removeprefix("./")


def category(path: str) -> str | None:
    core_paths = (
        "src/net/protocol.rs",
        "src/net/transition.rs",
        "src/net/framing.rs",
        "src/filetransfer/",
    )
    orchestration_paths = ("src/app/", "src/net/quic.rs", "src/setup/")
    if path.startswith(core_paths):
        return "Core"
    if path.startswith(orchestration_paths):
        return "Orchestration"
    return None


def parse_lcov(path: Path) -> dict[str, ModuleCoverage]:
    modules: dict[str, ModuleCoverage] = {}
    current: ModuleCoverage | None = None
    for raw_line in path.read_text(encoding="utf-8").splitlines():
        if raw_line.startswith("SF:"):
            source = repository_path(raw_line[3:])
            current = modules.setdefault(source, ModuleCoverage())
        elif raw_line == "end_of_record":
            current = None
        elif current is not None and raw_line.startswith("DA:"):
            line, hits, *_ = raw_line[3:].split(",")
            current.lines[int(line)] = current.lines.get(int(line), 0) + int(hits)
        elif current is not None and raw_line.startswith("FNDA:"):
            hits, name = raw_line[5:].split(",", 1)
            current.functions[name] = current.functions.get(name, 0) + int(hits)
    return modules


def percent(covered: int, total: int) -> str:
    return "n/a" if total == 0 else f"{covered * 100 / total:.1f}%"


def area_line_counts(
    modules: dict[str, ModuleCoverage], area: str
) -> tuple[int, int]:
    selected = [coverage for path, coverage in modules.items() if category(path) == area]
    return (
        sum(coverage.covered_lines for coverage in selected),
        sum(coverage.total_lines for coverage in selected),
    )


def threshold_failures(
    modules: dict[str, ModuleCoverage], thresholds: dict[str, float]
) -> list[str]:
    failures = []
    for area, minimum in thresholds.items():
        covered, total = area_line_counts(modules, area)
        actual = 0.0 if total == 0 else covered * 100 / total
        if actual < minimum:
            failures.append(f"{area} line coverage {actual:.1f}% is below {minimum:.1f}%")
    return failures


def render(modules: dict[str, ModuleCoverage]) -> str:
    selected = [(category(path), path, coverage) for path, coverage in modules.items()]
    selected = [entry for entry in selected if entry[0] is not None]
    selected.sort(key=lambda entry: (entry[0], entry[1]))

    lines = [
        "# Core and orchestration coverage",
        "",
        "| Area | Module | Lines | Line coverage | Functions | Function coverage |",
        "|---|---|---:|---:|---:|---:|",
    ]
    totals: dict[str, ModuleCoverage] = {}
    for area, path, coverage in selected:
        assert area is not None
        lines.append(
            f"| {area} | `{path}` | {coverage.covered_lines}/{coverage.total_lines} "
            f"| {percent(coverage.covered_lines, coverage.total_lines)} "
            f"| {coverage.covered_functions}/{coverage.total_functions} "
            f"| {percent(coverage.covered_functions, coverage.total_functions)} |"
        )
        total = totals.setdefault(area, ModuleCoverage())
        line_offset = len(total.lines)
        total.lines.update(
            {line_offset + index: hits for index, hits in enumerate(coverage.lines.values(), 1)}
        )
        function_offset = len(total.functions)
        total.functions.update(
            {
                f"{function_offset + index}:{name}": hits
                for index, (name, hits) in enumerate(coverage.functions.items(), 1)
            }
        )

    lines.extend(["", "## Area totals", "", "| Area | Line coverage | Function coverage |", "|---|---:|---:|"])
    for area in ("Core", "Orchestration"):
        coverage = totals.get(area, ModuleCoverage())
        lines.append(
            f"| {area} | {percent(coverage.covered_lines, coverage.total_lines)} "
            f"| {percent(coverage.covered_functions, coverage.total_functions)} |"
        )
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("lcov", type=Path)
    parser.add_argument("--fail-under-core-lines", type=float)
    parser.add_argument("--fail-under-orchestration-lines", type=float)
    args = parser.parse_args()
    modules = parse_lcov(args.lcov)
    print(render(modules), end="")

    thresholds = {
        area: minimum
        for area, minimum in (
            ("Core", args.fail_under_core_lines),
            ("Orchestration", args.fail_under_orchestration_lines),
        )
        if minimum is not None
    }
    failures = threshold_failures(modules, thresholds)
    if failures:
        for failure in failures:
            print(f"coverage threshold failed: {failure}", file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    main()
