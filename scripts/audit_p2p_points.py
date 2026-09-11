#!/usr/bin/env python3
"""P2P地震情報の551履歴と組み込み観測点TSVの名称一致を監査する。"""

import argparse
import json
from collections import Counter
from pathlib import Path


def load_stations(path: Path):
    stations = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        columns = line.split("\t")
        if len(columns) != 4:
            raise ValueError(f"TSVの列数が不正です: {line}")
        stations.add((columns[0], columns[1]))
    return stations


def audit(reports, stations):
    summary = {}
    for issue_type in sorted({r.get("issue", {}).get("type", "") for r in reports}):
        points = [
            point
            for report in reports
            if report.get("issue", {}).get("type", "") == issue_type
            for point in report.get("points", [])
        ]
        names = Counter(
            (point.get("pref", ""), point.get("addr", ""))
            for point in points
            if not point.get("isArea", False)
        )
        area_names = Counter(
            (point.get("pref", ""), point.get("addr", ""))
            for point in points
            if point.get("isArea", False)
        )
        area_count = sum(bool(point.get("isArea", False)) for point in points)
        station_names = {key for key in names if key in stations and key[1]}
        missing = sorted(key for key in names if key not in stations)
        summary[issue_type] = {
            "reports": sum(r.get("issue", {}).get("type", "") == issue_type for r in reports),
            "points": len(points),
            "unique_points": len(names),
            "is_area": area_count,
            "area_unique": len(area_names),
            "matched_unique": len(station_names),
            "missing_unique": len(missing),
            "missing": [(pref, name, names[(pref, name)]) for pref, name in missing],
        }
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="/history?codes=551 のJSON配列を保存したファイル")
    parser.add_argument("--stations", type=Path, default=Path("src/data/observation_stations.tsv"))
    args = parser.parse_args()
    reports = json.loads(args.input.read_text(encoding="utf-8"))
    summary = audit(reports, load_stations(args.stations))
    for issue_type, values in summary.items():
        print(
            f"{issue_type or '(不明)'}: 報数={values['reports']} "
            f"地点出現数={values['points']} 固有地点={values['unique_points']} "
            f"isArea={values['is_area']} 区域固有名={values['area_unique']} "
            f"観測点一致={values['matched_unique']} "
            f"観測点未一致={values['missing_unique']}"
        )
        for pref, name, count in values["missing"]:
            print(f"  未一致 {pref}\t{name}\t{count}件")


if __name__ == "__main__":
    main()
