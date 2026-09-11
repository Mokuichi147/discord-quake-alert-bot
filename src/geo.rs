//! 観測点(市区町村)・都道府県の座標テーブルと、観測点からマーカー一覧への変換。
//!
//! P2P地震情報の `points` には座標が含まれないため、地図を描くには地点名から
//! 座標を引く必要がある。`addr`（観測点名。例: "八戸市湊町"）が
//! `data/observation_stations.tsv` の都道府県・観測点名に一致すれば、
//! 公開データの座標にプロットする（詳報 `DetailScale` はこの粒度）。
//! 一致しない地点（震度速報の地域名や未収録の観測点など）は都道府県の代表座標へ
//! フォールバックする。

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{ensure, Result};

use crate::intensity::{display_pref, eew_area_scale, normalize_pref};
use crate::model::{EewArea, Point};

/// 気象庁公開の観測点マップから生成した都道府県・観測点名・緯度・経度のTSV。
/// 気象庁・地方公共団体・防災科学技術研究所の観測点を含む。
/// 出典・精度・更新手順は data/README.md を参照。
const OBSERVATION_STATIONS_TSV: &str = include_str!("data/observation_stations.tsv");

/// 組み込み観測点の検索・表示に使う1地点。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObservationStation {
    pub pref: &'static str,
    pub name: &'static str,
    pub latitude: f64,
    pub longitude: f64,
}

type StationCoords = HashMap<(&'static str, &'static str), (f64, f64)>;

/// 組み込み観測点TSVをパースした一覧。
pub fn observation_stations() -> &'static [ObservationStation] {
    static CACHE: OnceLock<Vec<ObservationStation>> = OnceLock::new();
    CACHE.get_or_init(|| {
        OBSERVATION_STATIONS_TSV
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| {
                let cols: Vec<_> = line.split('\t').collect();
                assert_eq!(cols.len(), 4, "組み込み観測点TSVの列数が不正です");
                ObservationStation {
                    pref: cols[0],
                    name: cols[1],
                    latitude: cols[2].parse().expect("観測点の緯度が不正です"),
                    longitude: cols[3].parse().expect("観測点の経度が不正です"),
                }
            })
            .collect()
    })
}

/// 都道府県と観測点名の組み合わせで照合し、別の県の同名地点を混同しない。
fn station_coords() -> &'static StationCoords {
    static CACHE: OnceLock<StationCoords> = OnceLock::new();
    CACHE.get_or_init(|| {
        observation_stations()
            .iter()
            .map(|station| {
                (
                    (normalize_pref(station.pref), station.name),
                    (station.latitude, station.longitude),
                )
            })
            .collect()
    })
}

/// 都道府県・観測点名から公開データの座標を引く。未収録の地点は None。
pub fn observation_point_coord(pref: &str, addr: &str) -> Option<(f64, f64)> {
    station_coords().get(&(normalize_pref(pref), addr)).copied()
}

/// 検索用に全角英数字・空白・区切り記号の違いを吸収する。
fn normalize_search_text(value: &str) -> String {
    let mut normalized = String::new();
    for character in value.chars() {
        let character = match character {
            'Ａ'..='Ｚ' => char::from_u32(character as u32 - 'Ａ' as u32 + 'A' as u32)
                .expect("全角英字の変換に失敗しました"),
            'ａ'..='ｚ' => char::from_u32(character as u32 - 'ａ' as u32 + 'a' as u32)
                .expect("全角英字の変換に失敗しました"),
            '０'..='９' => char::from_u32(character as u32 - '０' as u32 + '0' as u32)
                .expect("全角数字の変換に失敗しました"),
            '　' => ' ',
            other => other,
        };
        for character in character.to_lowercase() {
            if !character.is_whitespace()
                && !matches!(
                    character,
                    '・' | '･' | '-' | '‐' | '‑' | '‒' | '–' | '—' | '―' | '_'
                )
            {
                normalized.push(character);
            }
        }
    }
    normalized
}

/// 地域名の一部から観測点名を検索する結果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObservationStationMatch {
    pub score: f64,
    pub station: ObservationStation,
}

/// 地域名の一部や都道府県付きの表記から観測点候補を検索する。
///
/// 結果の `station.name` は入力に合わせて変換せず、情報源との照合に使える原文を返す。
pub fn search_observation_stations(
    query: &str,
    limit: Option<usize>,
) -> Result<Vec<ObservationStationMatch>> {
    if let Some(limit) = limit {
        ensure!(limit > 0, "limitは1以上で指定してください");
    }
    let query = normalize_search_text(query);
    ensure!(!query.is_empty(), "検索語を1文字以上で指定してください");

    let mut matches = observation_stations()
        .iter()
        .copied()
        .filter_map(|station| {
            let name = normalize_search_text(station.name);
            let pref = normalize_search_text(station.pref);
            let pref_stem = pref
                .strip_suffix('都')
                .or_else(|| pref.strip_suffix('府'))
                .or_else(|| pref.strip_suffix('県'))
                .unwrap_or(&pref);
            let mut combined_forms = vec![format!("{pref}{name}")];
            if !pref_stem.is_empty() && name.starts_with(pref_stem) {
                let alias = format!("{pref}{}", &name[pref_stem.len()..]);
                if !combined_forms.iter().any(|form| form == &alias) {
                    combined_forms.push(alias);
                }
            }

            let score = if query == name {
                1.0
            } else if name.contains(&query) {
                0.70 + 0.30 * query.chars().count() as f64 / name.chars().count() as f64
            } else if query == pref {
                0.65
            } else if let Some(combined) = combined_forms.iter().find(|form| form.contains(&query))
            {
                0.50 + 0.15 * query.chars().count() as f64 / combined.chars().count() as f64
            } else {
                return None;
            };
            Some(ObservationStationMatch { score, station })
        })
        .collect::<Vec<_>>();

    matches.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.station.name.len().cmp(&right.station.name.len()))
            .then_with(|| left.station.pref.cmp(right.station.pref))
            .then_with(|| left.station.name.cmp(right.station.name))
    });
    if let Some(limit) = limit {
        matches.truncate(limit);
    }
    Ok(matches)
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
/// `isArea=false` の `addr` が観測点座標テーブルに一致する地点は、その観測点の公開座標に
/// 個別のマーカーとしてプロットする。`isArea=true` の区域名や一致しない地点は
/// 都道府県ごとにまとめ、代表座標へフォールバックする（同一県は最大震度を採用）。
/// 代表座標も引けない県（海外・離島の予報区など）は除外する。
pub fn points_to_markers(points: &[Point]) -> Vec<(f64, f64, i32)> {
    let mut markers: Vec<(f64, f64, i32)> = Vec::new();
    let mut fallback_max_by_pref: HashMap<&str, i32> = HashMap::new();

    for p in points {
        if p.scale < 0 || p.pref.is_empty() {
            continue;
        }
        if !p.is_area {
            if let Some((lat, lon)) = observation_point_coord(&p.pref, &p.addr) {
                markers.push((lat, lon, p.scale));
                continue;
            }
        }
        let entry = fallback_max_by_pref
            .entry(p.pref.as_str())
            .or_insert(p.scale);
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
/// `points_to_markers` の 556 版。`areas.name` は観測点ではなく府県予報区の細分区域名
/// （例: "神奈川県西部"）なので観測点座標テーブルとは照合せず、都道府県の代表座標へ
/// フォールバックする（同一県は最大予想震度を採用）。
pub fn eew_areas_to_markers(areas: &[EewArea]) -> Vec<(f64, f64, i32)> {
    let mut markers: Vec<(f64, f64, i32)> = Vec::new();
    let mut fallback_max_by_pref: HashMap<String, i32> = HashMap::new();

    for a in areas {
        // 「〜程度以上」(99) は上限が不明なので下限を代表値にする。
        let scale = eew_area_scale(a);
        // 震度0（揺れを感じない）は描いても情報にならないので除外する。
        // 551 の points には 0 が無いため、この判定は 556 側にだけ必要。
        if scale <= 0 || a.pref.is_empty() {
            continue;
        }
        let pref = display_pref(&a.pref);
        let entry = fallback_max_by_pref.entry(pref).or_insert(scale);
        if scale > *entry {
            *entry = scale;
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
            is_area: false,
            scale,
        }
    }

    fn pt_addr(pref: &str, addr: &str, scale: i32) -> Point {
        Point {
            pref: pref.to_string(),
            addr: addr.to_string(),
            is_area: false,
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
        assert!(observation_point_coord("青森県", "八戸市湊町").is_some());
        assert!(observation_point_coord("青森県", "存在しない観測点").is_none());
    }

    #[test]
    fn expanded_stations_include_local_government_and_nied() {
        // 公開JSONに収録された地方公共団体・防災科学技術研究所の観測点。
        assert_eq!(
            observation_point_coord("北海道", "新篠津村第４７線"),
            Some((43.23, 141.65))
        );
        assert_eq!(
            observation_point_coord("北海道", "石狩市厚田"),
            Some((43.40, 141.43))
        );
        let markers = points_to_markers(&[
            pt_addr("北海道", "新篠津村第４７線", 30),
            pt_addr("北海道", "石狩市厚田", 40),
        ]);
        assert_eq!(markers, vec![(43.23, 141.65, 30), (43.40, 141.43, 40)]);
    }

    #[test]
    fn bundled_coordinate_uses_public_json_value() {
        assert_eq!(
            observation_point_coord("北海道", "石狩市花川"),
            Some((43.17, 141.32))
        );
    }

    #[test]
    fn station_lookup_requires_matching_prefecture() {
        assert_eq!(
            observation_point_coord("青森", "八戸市湊町"),
            observation_point_coord("青森県", "八戸市湊町")
        );
        assert!(observation_point_coord("岩手県", "八戸市湊町").is_none());
        assert!(observation_point_coord("", "八戸市湊町").is_none());
        let markers = points_to_markers(&[pt_addr("岩手県", "八戸市湊町", 40)]);
        assert_eq!((markers[0].0, markers[0].1), pref_coord("岩手県").unwrap());
    }

    #[test]
    fn station_search_returns_source_names_for_partial_region() {
        let matches = search_observation_stations("東京都千代田区", Some(3)).unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].station.pref, "東京都");
        assert!(matches
            .iter()
            .all(|matched| matched.station.name.contains("東京千代田区")));
        assert!(matches[0].score >= matches[1].score);
    }

    #[test]
    fn station_search_normalizes_whitespace_and_rejects_empty_query() {
        let matches = search_observation_stations("東京　千代田区", Some(1)).unwrap();
        assert_eq!(matches[0].station.name, "東京千代田区麹町");
        assert!(search_observation_stations(" ", Some(5)).is_err());
        assert!(search_observation_stations("東京", Some(0)).is_err());
        assert!(search_observation_stations("東京", None).unwrap().len() > 5);
    }

    #[test]
    fn area_named_like_a_station_is_still_not_plotted_as_a_station() {
        // 551 の `isArea=true` は区域名であり、文字列が偶然観測点名と同じでも
        // 観測点の座標へ誤って置かない。
        let area = Point {
            pref: "青森県".to_string(),
            addr: "八戸市湊町".to_string(),
            is_area: true,
            scale: 40,
        };
        let markers = points_to_markers(&[area]);
        assert_eq!(markers.len(), 1);
        assert_eq!((markers[0].0, markers[0].1), pref_coord("青森県").unwrap());
    }

    #[test]
    fn bundled_stations_are_valid_and_unique() {
        let mut keys = std::collections::HashSet::new();
        for line in OBSERVATION_STATIONS_TSV
            .lines()
            .filter(|line| !line.starts_with('#'))
        {
            let cols: Vec<_> = line.split('\t').collect();
            assert_eq!(cols.len(), 4);
            assert!(pref_coord(cols[0]).is_some(), "未知の都道府県: {line}");
            assert!(!cols[1].is_empty());
            assert!(keys.insert((cols[0], cols[1])), "重複: {line}");
            let lat: f64 = cols[2].parse().unwrap();
            let lon: f64 = cols[3].parse().unwrap();
            assert!((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon));
            assert_ne!((lat, lon), (0.0, 0.0));
        }
        assert!(keys.len() > 4000);
        assert_eq!(keys.len(), station_coords().len());
    }

    #[test]
    fn markers_use_observation_point_coord_when_addr_matches() {
        // addr が観測点座標テーブルに一致する場合は、県代表座標ではなく
        // その観測点の公開座標を個別マーカーとして使う。
        let points = vec![pt_addr("青森県", "八戸市湊町", 40)];
        let markers = points_to_markers(&points);
        assert_eq!(markers.len(), 1);
        let station_coord = observation_point_coord("青森県", "八戸市湊町").unwrap();
        assert_eq!((markers[0].0, markers[0].1), station_coord);
        assert_ne!((markers[0].0, markers[0].1), pref_coord("青森県").unwrap());
        assert_eq!(markers[0].2, 40);
    }

    #[test]
    fn markers_fall_back_to_pref_when_addr_unmatched() {
        // 震度速報などの地域名（addr）は座標テーブルに無いため、県代表座標に集約される。
        let points = vec![
            pt_addr("宮城県", "宮城県北部", 45),
            pt_addr("宮城県", "宮城県南部", 30),
        ];
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
        let areas = vec![
            area("神奈川", "神奈川県西部", 45),
            area("神奈川", "神奈川県東部", 40),
        ];
        let markers = eew_areas_to_markers(&areas);
        assert_eq!(markers.len(), 1);
        assert_eq!(
            (markers[0].0, markers[0].1),
            pref_coord("神奈川県").unwrap()
        );
        assert_eq!(markers[0].2, 45); // 最大予想震度を採用
    }

    #[test]
    fn eew_markers_take_max_scale_per_pref() {
        let areas = vec![
            area("神奈川", "神奈川県西部", 40),
            area("大阪", "大阪府北部", 30),
        ];
        let mut markers = eew_areas_to_markers(&areas);
        markers.sort_by_key(|m| m.2);
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].2, 30);
        assert_eq!(markers[1].2, 40);
    }

    #[test]
    fn eew_markers_use_lower_bound_for_unbounded_scale() {
        // scale_to=99（〜程度以上）は下限でプロットする。99 のままだと
        // 震度7より濃い色になり、実際の予想（5弱程度以上）と食い違う。
        let areas = vec![EewArea {
            pref: "熊本".to_string(),
            name: "熊本県熊本".to_string(),
            scale_from: 45,
            scale_to: 99,
        }];
        let markers = eew_areas_to_markers(&areas);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].2, 45);
    }

    #[test]
    fn eew_markers_skip_invalid_scale() {
        let areas = vec![area("神奈川", "神奈川県西部", -1)];
        assert!(eew_areas_to_markers(&areas).is_empty());
    }

    #[test]
    fn eew_markers_skip_shindo0() {
        // 震度0（揺れを感じない）は描いても情報にならないので除外する。
        let areas = vec![area("神奈川", "神奈川県西部", 0)];
        assert!(eew_areas_to_markers(&areas).is_empty());
        // 震度1以上は描く。
        assert_eq!(
            eew_areas_to_markers(&[area("神奈川", "神奈川県西部", 10)]).len(),
            1
        );
    }
}
