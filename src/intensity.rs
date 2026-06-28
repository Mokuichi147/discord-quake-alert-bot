//! 震度スケールの変換と、通知条件の判定ロジック。

use std::collections::HashMap;

use crate::model::{Earthquake, EewArea, Point};

/// 地方区分。`(地方名, 環境変数の接頭辞, 含む都道府県の正規化形)`。
///
/// 正規化形は末尾の「都/府/県」を除いた表記（北海道は「道」を残す）。これにより
/// 551 の `points.pref`（"東京都"）と 556 の `areas.pref`（"東京"）の両方を同一キーで扱える。
pub const REGIONS: &[(&str, &str, &[&str])] = &[
    ("北海道", "HOKKAIDO", &["北海道"]),
    ("東北", "TOHOKU", &["青森", "岩手", "宮城", "秋田", "山形", "福島"]),
    ("関東", "KANTO", &["茨城", "栃木", "群馬", "埼玉", "千葉", "東京", "神奈川"]),
    (
        "中部",
        "CHUBU",
        &["新潟", "富山", "石川", "福井", "山梨", "長野", "岐阜", "静岡", "愛知"],
    ),
    ("近畿", "KINKI", &["三重", "滋賀", "京都", "大阪", "兵庫", "奈良", "和歌山"]),
    ("中国", "CHUGOKU", &["鳥取", "島根", "岡山", "広島", "山口"]),
    ("四国", "SHIKOKU", &["徳島", "香川", "愛媛", "高知"]),
    (
        "九州",
        "KYUSHU",
        &["福岡", "佐賀", "長崎", "熊本", "大分", "宮崎", "鹿児島", "沖縄"],
    ),
];

/// 都道府県名を正規化する。末尾の「都/府/県」を除く（「道」は残す）。
/// 例: "東京都"→"東京", "大阪府"→"大阪", "北海道"→"北海道", "東京"→"東京"。
pub fn normalize_pref(pref: &str) -> &str {
    pref.strip_suffix('都')
        .or_else(|| pref.strip_suffix('府'))
        .or_else(|| pref.strip_suffix('県'))
        .unwrap_or(pref)
}

/// 都道府県(正規化前でも可)が属する地方名を返す。該当なしは None。
pub fn region_of(pref: &str) -> Option<&'static str> {
    let np = normalize_pref(pref);
    REGIONS
        .iter()
        .find(|(_, _, prefs)| prefs.contains(&np))
        .map(|(name, _, _)| *name)
}

/// 地方の通知下限を返す。設定がなければ `other_min_scale` にフォールバックする。
fn region_threshold(
    region: Option<&str>,
    region_min_scales: &HashMap<String, i32>,
    other_min_scale: i32,
) -> i32 {
    region
        .and_then(|r| region_min_scales.get(r))
        .copied()
        .unwrap_or(other_min_scale)
}

/// P2P地震情報のscale値を震度表記へ変換する。
pub fn scale_label(scale: i32) -> &'static str {
    match scale {
        10 => "1",
        20 => "2",
        30 => "3",
        40 => "4",
        45 => "5弱",
        50 => "5強",
        55 => "6弱",
        60 => "6強",
        70 => "7",
        _ => "不明",
    }
}

/// 震度に応じた埋め込みカラー（揺れが強いほど赤系）。
pub fn embed_color(scale: i32) -> u32 {
    match scale {
        s if s >= 60 => 0x8B_00_00, // 6強・7: 濃い赤
        55 => 0xE0_00_00,           // 6弱
        50 => 0xFF_44_00,           // 5強
        45 => 0xFF_88_00,           // 5弱
        40 => 0xFF_C0_00,           // 4
        _ => 0x33_99_FF,            // 3以下・不明
    }
}

/// 地図マーカー色 (R, G, B)。
pub fn marker_rgb(scale: i32) -> (u8, u8, u8) {
    match scale {
        s if s >= 60 => (139, 0, 0),
        55 => (224, 0, 0),
        50 => (255, 68, 0),
        45 => (255, 136, 0),
        40 => (255, 192, 0),
        _ => (51, 153, 255),
    }
}

/// 国内津波コードを日本語表記へ。
pub fn tsunami_label(code: &str) -> &'static str {
    match code {
        "None" => "なし",
        "NonEffective" => "若干の海面変動（被害の心配なし）",
        "Watch" => "津波注意報",
        "Warning" => "津波警報・大津波警報",
        "Checking" => "調査中",
        _ => "不明",
    }
}

/// 津波に関する情報があるか（海面変動・注意報・警報・調査中）。
/// 「なし(None)」「不明(Unknown 等)」は false。
pub fn has_tsunami(code: &str) -> bool {
    matches!(code, "NonEffective" | "Watch" | "Warning" | "Checking")
}

/// 津波予報(552)の種別(`grade`)を日本語表記へ。
pub fn tsunami_grade_label(grade: &str) -> &'static str {
    match grade {
        "MajorWarning" => "大津波警報",
        "Warning" => "津波警報",
        "Watch" => "津波注意報",
        _ => "津波予報",
    }
}

/// 津波予報の重大度ランク（高いほど深刻）。最大 grade の選定に使う。
pub fn tsunami_grade_rank(grade: &str) -> i32 {
    match grade {
        "MajorWarning" => 3,
        "Warning" => 2,
        "Watch" => 1,
        _ => 0,
    }
}

/// 津波予報の embed カラー（深刻なほど濃い色）。
pub fn tsunami_grade_color(grade: &str) -> u32 {
    match grade {
        "MajorWarning" => 0x99_00_99, // 大津波警報: 紫
        "Warning" => 0xE0_00_00,      // 津波警報: 赤
        "Watch" => 0xFF_C0_00,        // 津波注意報: 黄
        _ => 0x33_99_FF,
    }
}

/// 通知判定結果。なぜ通知対象になったかの理由も保持する。
#[derive(Debug)]
pub struct NotifyDecision {
    pub notify: bool,
    pub reason: String,
}

/// 通知すべきかを判定する（地震情報 551 の観測震度版）。
///
/// 各地点を地方区分に分類し、その地方の下限（`region_min_scales`、未設定は
/// `other_min_scale` にフォールバック）以上の地点があれば通知する。
/// 地点情報がない場合は、全国最大震度を `other_min_scale` と比較する。
pub fn decide(
    eq: &Earthquake,
    points: &[Point],
    region_min_scales: &HashMap<String, i32>,
    other_min_scale: i32,
) -> NotifyDecision {
    // 地方しきい値を満たす地点のうち、最大の観測スケールを求める。
    let mut best_scale: Option<i32> = None;
    for p in points {
        let region = region_of(&p.pref);
        let threshold = region_threshold(region, region_min_scales, other_min_scale);
        if p.scale >= threshold {
            best_scale = Some(best_scale.map_or(p.scale, |s| s.max(p.scale)));
        }
    }

    if let Some(scale) = best_scale {
        // 通知ポップアップでも場所が分かるよう、その最大震度を観測した都道府県名で示す。
        let place = format_place(&prefs_at_scale(points, scale));
        return NotifyDecision {
            notify: true,
            reason: format!("{place}で最大震度{}を観測", scale_label(scale)),
        };
    }

    // 地点情報がない場合のフォールバック。
    if eq.max_scale >= other_min_scale {
        return NotifyDecision {
            notify: true,
            reason: format!("最大震度{}を観測", scale_label(eq.max_scale)),
        };
    }

    NotifyDecision {
        notify: false,
        reason: String::new(),
    }
}

/// 指定スケールちょうどを観測した都道府県を、出現順・重複なしで返す。
fn prefs_at_scale(points: &[Point], scale: i32) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for p in points {
        if p.scale == scale && !p.pref.is_empty() && !out.contains(&p.pref.as_str()) {
            out.push(p.pref.as_str());
        }
    }
    out
}

/// 都道府県名の一覧を表示用にまとめる（最大4件、超過分は「など」に畳む）。
fn format_place(prefs: &[&str]) -> String {
    const MAX: usize = 4;
    if prefs.is_empty() {
        return "各地".to_string();
    }
    let shown = prefs.len().min(MAX);
    let mut place = prefs[..shown].join("・");
    if prefs.len() > shown {
        place.push_str("など");
    }
    place
}

/// 緊急地震速報(556)の通知判定。`decide` の予想震度版。
///
/// 各地域を地方区分に分類し、地方の下限（未設定は `other_min_scale`）以上の
/// 予想震度（`scale_to`）があれば通知する。
pub fn decide_eew(
    areas: &[EewArea],
    region_min_scales: &HashMap<String, i32>,
    other_min_scale: i32,
) -> NotifyDecision {
    let mut best: Option<(&str, i32)> = None;
    for a in areas {
        let region = region_of(&a.pref);
        let threshold = region_threshold(region, region_min_scales, other_min_scale);
        if a.scale_to >= threshold && best.is_none_or(|(_, s)| a.scale_to > s) {
            best = Some((region.unwrap_or("全国"), a.scale_to));
        }
    }

    if let Some((label, scale)) = best {
        return NotifyDecision {
            notify: true,
            reason: format!("{label}で予想最大震度{}", scale_label(scale)),
        };
    }

    NotifyDecision {
        notify: false,
        reason: String::new(),
    }
}

/// 緊急地震速報の予想最大震度（全地域の `scale_to` 最大）を返す。
pub fn eew_max_scale(areas: &[EewArea]) -> i32 {
    areas.iter().map(|a| a.scale_to).max().unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Point;

    fn pt(pref: &str, scale: i32) -> Point {
        Point {
            pref: pref.to_string(),
            addr: String::new(),
            scale,
        }
    }

    /// 関東を 40 に設定した場合の地方下限（他地方は未設定）。
    fn kanto40() -> HashMap<String, i32> {
        HashMap::from([("関東".to_string(), 40)])
    }

    #[test]
    fn normalize_and_region() {
        assert_eq!(normalize_pref("東京都"), "東京");
        assert_eq!(normalize_pref("大阪府"), "大阪");
        assert_eq!(normalize_pref("北海道"), "北海道");
        assert_eq!(normalize_pref("東京"), "東京");
        assert_eq!(region_of("東京都"), Some("関東"));
        assert_eq!(region_of("宮城"), Some("東北"));
        assert_eq!(region_of("大阪府"), Some("近畿"));
        assert_eq!(region_of("ハワイ"), None);
    }

    #[test]
    fn kanto_shindo4_notifies() {
        let eq = Earthquake {
            max_scale: 40,
            ..Default::default()
        };
        let points = vec![pt("東京都", 40), pt("大阪府", 30)];
        let d = decide(&eq, &points, &kanto40(), 50);
        assert!(d.notify);
        // 理由文は地方名ではなく観測した都道府県名で示す。
        assert!(d.reason.contains("東京都"), "{}", d.reason);
        assert!(d.reason.contains("最大震度4"), "{}", d.reason);
    }

    #[test]
    fn kanto_shindo3_does_not_notify() {
        let eq = Earthquake {
            max_scale: 30,
            ..Default::default()
        };
        let points = vec![pt("千葉県", 30)];
        assert!(!decide(&eq, &points, &kanto40(), 50).notify);
    }

    #[test]
    fn other_region_needs_5kyo() {
        let eq = Earthquake {
            max_scale: 40,
            ..Default::default()
        };
        // 関東以外で震度4のみ → 通知しない（北海道は未設定なので50が下限）
        let points = vec![pt("北海道", 40)];
        assert!(!decide(&eq, &points, &kanto40(), 50).notify);

        // 関東以外で震度5強 → 通知する
        let eq2 = Earthquake {
            max_scale: 50,
            ..Default::default()
        };
        let points2 = vec![pt("北海道", 50)];
        assert!(decide(&eq2, &points2, &kanto40(), 50).notify);
    }

    #[test]
    fn tohoku_threshold_applies() {
        let eq = Earthquake {
            max_scale: 40,
            ..Default::default()
        };
        // 東北を40に設定 → 宮城の震度4で通知
        let scales = HashMap::from([("関東".to_string(), 40), ("東北".to_string(), 40)]);
        let d = decide(&eq, &[pt("宮城県", 40)], &scales, 50);
        assert!(d.notify);
        assert!(d.reason.contains("宮城県"), "{}", d.reason);
        // 東北を未設定にすると同じ震度4では通知しない（フォールバック50）
        assert!(!decide(&eq, &[pt("宮城県", 40)], &kanto40(), 50).notify);
    }

    #[test]
    fn reason_lists_prefs_at_max_scale() {
        let eq = Earthquake {
            max_scale: 45,
            ..Default::default()
        };
        let scales = HashMap::from([("東北".to_string(), 40)]);
        // 最大震度5弱(45)を観測したのは青森県・岩手県。宮城県は震度3で対象外。
        let points = vec![
            pt("青森県", 45),
            pt("岩手県", 45),
            pt("岩手県", 40),
            pt("宮城県", 30),
        ];
        let d = decide(&eq, &points, &scales, 50);
        assert!(d.notify);
        assert_eq!(d.reason, "青森県・岩手県で最大震度5弱を観測");
    }

    fn area(pref: &str, scale_to: i32) -> EewArea {
        EewArea {
            pref: pref.to_string(),
            name: String::new(),
            scale_from: scale_to,
            scale_to,
        }
    }

    #[test]
    fn eew_kanto_yosou4_notifies() {
        // 関東(接尾辞なしpref)で予想震度4 → 通知
        let areas = vec![area("神奈川", 40), area("大阪", 30)];
        let d = decide_eew(&areas, &kanto40(), 50);
        assert!(d.notify);
        assert!(d.reason.contains("関東"));
    }

    #[test]
    fn eew_other_region_needs_5kyo() {
        // 関東外で予想震度4のみ → 通知しない
        assert!(!decide_eew(&[area("北海道", 40)], &kanto40(), 50).notify);
        // 関東外で予想震度5強 → 通知する
        assert!(decide_eew(&[area("北海道", 50)], &kanto40(), 50).notify);
    }

    #[test]
    fn eew_max_scale_picks_highest() {
        let areas = vec![area("山梨", 45), area("神奈川", 40), area("静岡", 40)];
        assert_eq!(eew_max_scale(&areas), 45);
    }
}
