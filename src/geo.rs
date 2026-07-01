//! 観測点(市区町村)・都道府県の座標テーブルと、観測点からマーカー一覧への変換。
//!
//! P2P地震情報の `points` には座標が含まれないため、地図を描くには地点名から
//! 座標を引く必要がある。`addr`（観測点名。例: "八戸市湊町"）が
//! `data/observation_points.tsv` の座標テーブルに一致すれば、その市区町村・観測点の
//! 正確な座標にプロットする（詳報 `DetailScale` はこの粒度）。一致しない地点
//! （震度速報の地域名や、気象庁以外が運用する観測点など）は都道府県の代表座標へ
//! フォールバックする。

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::intensity::display_pref;
use crate::model::{EewArea, Point};

/// 観測点名 → (緯度, 経度) の座標データ（タブ区切り: 名前\t緯度\t経度）。
///
/// 気象庁公式サイトの「[気象庁震度観測点一覧表](https://www.data.jma.go.jp/eqev/data/kyoshin/jma-shindo.html)」
/// （現用の観測点のみ、CC BY 4.0）から抽出した気象庁自身の観測点データ。
/// 都道府県・市区町村や防災科学技術研究所(NIED)が独自に運用する観測点は、
/// それぞれ利用規約が異なる（NIEDは再配布を禁止している）ため含めていない。
/// そのため `points.addr` がこのテーブルに一致するのは気象庁自身の観測点のみで、
/// 一致しない地点は都道府県代表座標にフォールバックする。
/// 観測点の統廃合により将来的にズレが生じる可能性がある。
const OBSERVATION_POINTS_TSV: &str = include_str!("data/observation_points.tsv");

/// 観測点名 → 座標のルックアップテーブルを初回アクセス時にパースして返す。
fn observation_points() -> &'static HashMap<&'static str, (f64, f64)> {
    static CACHE: OnceLock<HashMap<&'static str, (f64, f64)>> = OnceLock::new();
    CACHE.get_or_init(|| {
        OBSERVATION_POINTS_TSV
            .lines()
            .filter_map(|line| {
                let mut cols = line.split('\t');
                let name = cols.next()?;
                let lat: f64 = cols.next()?.parse().ok()?;
                let lon: f64 = cols.next()?.parse().ok()?;
                Some((name, (lat, lon)))
            })
            .collect()
    })
}

/// 観測点名（`points.addr`）から正確な座標 (緯度, 経度) を引く。未収録の地点は None。
pub fn observation_point_coord(addr: &str) -> Option<(f64, f64)> {
    observation_points().get(addr).copied()
}

/// 都道府県名 → 代表座標 (緯度, 経度)。県名は 551 の `points.pref` の表記に合わせる。
const PREF_COORDS: &[(&str, f64, f64)] = &[
    ("北海道", 43.0642, 141.3469),
    ("青森県", 40.8244, 140.7400),
    ("岩手県", 39.7036, 141.1527),
    ("宮城県", 38.2688, 140.8721),
    ("秋田県", 39.7186, 140.1024),
    ("山形県", 38.2404, 140.3633),
    ("福島県", 37.7503, 140.4676),
    ("茨城県", 36.3418, 140.4468),
    ("栃木県", 36.5657, 139.8836),
    ("群馬県", 36.3907, 139.0604),
    ("埼玉県", 35.8570, 139.6489),
    ("千葉県", 35.6051, 140.1233),
    ("東京都", 35.6895, 139.6917),
    ("神奈川県", 35.4478, 139.6425),
    ("新潟県", 37.9026, 139.0235),
    ("富山県", 36.6953, 137.2113),
    ("石川県", 36.5947, 136.6256),
    ("福井県", 36.0652, 136.2216),
    ("山梨県", 35.6642, 138.5684),
    ("長野県", 36.6513, 138.1810),
    ("岐阜県", 35.3912, 136.7223),
    ("静岡県", 34.9769, 138.3831),
    ("愛知県", 35.1802, 136.9066),
    ("三重県", 34.7303, 136.5086),
    ("滋賀県", 35.0045, 135.8686),
    ("京都府", 35.0212, 135.7556),
    ("大阪府", 34.6863, 135.5197),
    ("兵庫県", 34.6913, 135.1830),
    ("奈良県", 34.6851, 135.8329),
    ("和歌山県", 34.2261, 135.1675),
    ("鳥取県", 35.5039, 134.2380),
    ("島根県", 35.4723, 133.0505),
    ("岡山県", 34.6618, 133.9344),
    ("広島県", 34.3966, 132.4596),
    ("山口県", 34.1859, 131.4714),
    ("徳島県", 34.0658, 134.5593),
    ("香川県", 34.3401, 134.0434),
    ("愛媛県", 33.8417, 132.7657),
    ("高知県", 33.5597, 133.5311),
    ("福岡県", 33.6064, 130.4181),
    ("佐賀県", 33.2494, 130.2989),
    ("長崎県", 32.7448, 129.8737),
    ("熊本県", 32.7898, 130.7417),
    ("大分県", 33.2382, 131.6126),
    ("宮崎県", 31.9111, 131.4239),
    ("鹿児島県", 31.5602, 130.5581),
    ("沖縄県", 26.2124, 127.6809),
];

/// 都道府県名から代表座標 (緯度, 経度) を引く。未知の県は None。
pub fn pref_coord(pref: &str) -> Option<(f64, f64)> {
    PREF_COORDS
        .iter()
        .find(|(name, _, _)| *name == pref)
        .map(|(_, lat, lon)| (*lat, *lon))
}

/// 観測点を地図に描く `(緯度, 経度, 震度スケール)` の一覧へ変換する。
///
/// `addr` が観測点座標テーブルに一致する地点は、その市区町村・観測点の正確な座標に
/// 個別のマーカーとしてプロットする。一致しない地点（震度速報の地域名など）は
/// 都道府県ごとにまとめ、代表座標へフォールバックする（同一県は最大震度を採用）。
/// 代表座標も引けない県（海外・離島の予報区など）は除外する。
pub fn points_to_markers(points: &[Point]) -> Vec<(f64, f64, i32)> {
    let mut markers: Vec<(f64, f64, i32)> = Vec::new();
    let mut fallback_max_by_pref: HashMap<&str, i32> = HashMap::new();

    for p in points {
        if p.scale < 0 || p.pref.is_empty() {
            continue;
        }
        if let Some((lat, lon)) = observation_point_coord(&p.addr) {
            markers.push((lat, lon, p.scale));
            continue;
        }
        let entry = fallback_max_by_pref.entry(p.pref.as_str()).or_insert(p.scale);
        if p.scale > *entry {
            *entry = p.scale;
        }
    }

    markers.extend(
        fallback_max_by_pref
            .into_iter()
            .filter_map(|(pref, scale)| pref_coord(pref).map(|(lat, lon)| (lat, lon, scale))),
    );

    markers
}

/// 緊急地震速報(556)の対象地域を地図に描く `(緯度, 経度, 予想震度スケール)` の一覧へ変換する。
///
/// `points_to_markers` の 556 版。`areas.name`（地域名。例: "神奈川県西部"）が観測点座標
/// テーブルに一致すればその座標を使うが、556 の地域名は市区町村単位ではないため通常は
/// 一致せず、都道府県の代表座標へフォールバックする（同一県は最大予想震度を採用）。
pub fn eew_areas_to_markers(areas: &[EewArea]) -> Vec<(f64, f64, i32)> {
    let mut markers: Vec<(f64, f64, i32)> = Vec::new();
    let mut fallback_max_by_pref: HashMap<String, i32> = HashMap::new();

    for a in areas {
        if a.scale_to < 0 || a.pref.is_empty() {
            continue;
        }
        if let Some((lat, lon)) = observation_point_coord(&a.name) {
            markers.push((lat, lon, a.scale_to));
            continue;
        }
        let pref = display_pref(&a.pref);
        let entry = fallback_max_by_pref.entry(pref).or_insert(a.scale_to);
        if a.scale_to > *entry {
            *entry = a.scale_to;
        }
    }

    markers.extend(
        fallback_max_by_pref
            .into_iter()
            .filter_map(|(pref, scale)| pref_coord(&pref).map(|(lat, lon)| (lat, lon, scale))),
    );

    markers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(pref: &str, scale: i32) -> Point {
        Point {
            pref: pref.to_string(),
            addr: String::new(),
            scale,
        }
    }

    fn pt_addr(pref: &str, addr: &str, scale: i32) -> Point {
        Point {
            pref: pref.to_string(),
            addr: addr.to_string(),
            scale,
        }
    }

    #[test]
    fn coord_lookup() {
        assert!(pref_coord("宮城県").is_some());
        assert!(pref_coord("存在しない県").is_none());
    }

    #[test]
    fn observation_point_lookup() {
        assert!(observation_point_coord("八戸市湊町").is_some());
        assert!(observation_point_coord("存在しない観測点").is_none());
    }

    #[test]
    fn markers_use_observation_point_coord_when_addr_matches() {
        // addr が観測点座標テーブルに一致する場合は、県代表座標ではなく
        // その観測点の正確な座標を個別マーカーとして使う。
        let points = vec![pt_addr("青森県", "八戸市湊町", 40)];
        let markers = points_to_markers(&points);
        assert_eq!(markers.len(), 1);
        let station_coord = observation_point_coord("八戸市湊町").unwrap();
        assert_eq!((markers[0].0, markers[0].1), station_coord);
        assert_ne!((markers[0].0, markers[0].1), pref_coord("青森県").unwrap());
        assert_eq!(markers[0].2, 40);
    }

    #[test]
    fn markers_fall_back_to_pref_when_addr_unmatched() {
        // 震度速報などの地域名（addr）は座標テーブルに無いため、県代表座標に集約される。
        let points = vec![pt_addr("宮城県", "宮城県北部", 45), pt_addr("宮城県", "宮城県南部", 30)];
        let markers = points_to_markers(&points);
        assert_eq!(markers.len(), 1);
        assert_eq!((markers[0].0, markers[0].1), pref_coord("宮城県").unwrap());
        assert_eq!(markers[0].2, 45); // 最大震度を採用
    }

    #[test]
    fn markers_take_max_scale_per_pref() {
        let points = vec![pt("宮城県", 40), pt("宮城県", 45), pt("福島県", 30)];
        let mut markers = points_to_markers(&points);
        // 県の出現順は不定なので震度で整列して検証する。
        markers.sort_by_key(|m| m.2);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].2, 30); // 福島県
        assert_eq!(markers[1].2, 45); // 宮城県（最大震度を採用）
    }

    #[test]
    fn markers_skip_unknown_pref_and_invalid_scale() {
        let points = vec![pt("ハワイ", 50), pt("宮城県", -1)];
        assert!(points_to_markers(&points).is_empty());
    }

    fn area(pref: &str, name: &str, scale_to: i32) -> EewArea {
        EewArea {
            pref: pref.to_string(),
            name: name.to_string(),
            scale_from: scale_to,
            scale_to,
        }
    }

    #[test]
    fn eew_markers_fall_back_to_pref_representative_coord() {
        // 556 の地域名（例: "神奈川県西部"）は市区町村単位ではないため、
        // 通常は観測点座標テーブルに一致せず県代表座標に集約される。
        let areas = vec![area("神奈川", "神奈川県西部", 45), area("神奈川", "神奈川県東部", 40)];
        let markers = eew_areas_to_markers(&areas);
        assert_eq!(markers.len(), 1);
        assert_eq!((markers[0].0, markers[0].1), pref_coord("神奈川県").unwrap());
        assert_eq!(markers[0].2, 45); // 最大予想震度を採用
    }

    #[test]
    fn eew_markers_take_max_scale_per_pref() {
        let areas = vec![area("神奈川", "神奈川県西部", 40), area("大阪", "大阪府北部", 30)];
        let mut markers = eew_areas_to_markers(&areas);
        markers.sort_by_key(|m| m.2);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].2, 30);
        assert_eq!(markers[1].2, 40);
    }

    #[test]
    fn eew_markers_skip_invalid_scale() {
        let areas = vec![area("神奈川", "神奈川県西部", -1)];
        assert!(eew_areas_to_markers(&areas).is_empty());
    }
}
