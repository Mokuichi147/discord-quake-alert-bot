#!/usr/bin/env python3
"""気象庁の公開観測点JSONを検証し、botに組み込むTSVを生成する。"""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
from pathlib import Path
import urllib.request

SOURCE = "https://ds.data.jma.go.jp/eqev/data/intens-st/stations.json"
ROOT = Path(__file__).resolve().parents[1]
PREFS = (
    "北海道 青森県 岩手県 宮城県 秋田県 山形県 福島県 茨城県 栃木県 群馬県 "
    "埼玉県 千葉県 東京都 神奈川県 新潟県 富山県 石川県 福井県 山梨県 長野県 "
    "岐阜県 静岡県 愛知県 三重県 滋賀県 京都府 大阪府 兵庫県 奈良県 和歌山県 "
    "鳥取県 島根県 岡山県 広島県 山口県 徳島県 香川県 愛媛県 高知県 福岡県 "
    "佐賀県 長崎県 熊本県 大分県 宮崎県 鹿児島県 沖縄県"
).split()


def format_coord(value):
    """浮動小数点の内部表現を出さず、必要な桁だけで出力する。"""
    return f"{value:.6f}".rstrip("0").rstrip(".")


def convert(stations):
    """重複・不正値は書き込み前に拒否し、登録順によらない出力にする。"""
    if not isinstance(stations, list) or not stations:
        raise ValueError("観測点データは空でない配列である必要があります")
    rows = {}
    for station in stations:
        pref_code = int(station["pref"])
        affiliation = int(station["affi"])
        name = station["name"]
        lat, lon = float(station["lat"]), float(station["lon"])
        if not 1 <= pref_code <= 47 or affiliation not in (0, 1, 2):
            raise ValueError("不正な都道府県コード・所属です")
        if not isinstance(name, str) or not name or any(c.isspace() for c in name):
            raise ValueError("不正な観測点名です")
        if not (math.isfinite(lat) and math.isfinite(lon)
                and -90 <= lat <= 90 and -180 <= lon <= 180
                and (lat, lon) != (0, 0)):
            raise ValueError(f"不正な座標: {name}")
        key = (pref_code, name)
        if key in rows:
            raise ValueError(f"都道府県・観測点名の重複: {key}")
        rows[key] = (lat, lon)
    lines = [
        f"{PREFS[code - 1]}\t{name}\t{format_coord(lat)}\t{format_coord(lon)}"
        for (code, name), (lat, lon) in sorted(rows.items())
    ]
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, help="取得済みJSON（省略時は公開URLから取得）")
    parser.add_argument("--output", type=Path, default=ROOT / "src/data/observation_stations.tsv")
    args = parser.parse_args()
    if args.input:
        raw = args.input.read_bytes()
    else:
        with urllib.request.urlopen(SOURCE, timeout=30) as response:
            raw = response.read()
    lines = convert(json.loads(raw))
    header = (
        f"# 出典: 気象庁 震度観測点 {SOURCE}\n"
        "# quake-alert-botが都道府県名へ変換・整列（座標は公開JSONの値を使用）\n"
        f"# 生成日(UTC): {datetime.now(timezone.utc).date()} / 入力SHA-256: {hashlib.sha256(raw).hexdigest()}\n"
        f"# 観測点数: {len(lines)}\n"
        "# 都道府県\t観測点名\t緯度\t経度\n"
    )
    # 取得・解析・検証がすべて成功してから置換する。
    temporary = args.output.with_suffix(".tmp")
    temporary.write_text(header + "\n".join(lines) + "\n", encoding="utf-8")
    temporary.replace(args.output)
    print(f"{len(lines)}地点を保存しました: {args.output}")


if __name__ == "__main__":
    main()
