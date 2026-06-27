//! 震度スケールの変換と、通知条件の判定ロジック。

use crate::model::{Earthquake, EewArea, Point};

/// 関東地方の都道府県（551 の `points.pref` 表記。接尾辞あり）。
pub const KANTO_PREFS: [&str; 7] = [
    "茨城県",
    "栃木県",
    "群馬県",
    "埼玉県",
    "千葉県",
    "東京都",
    "神奈川県",
];

/// 関東地方の都府県（緊急地震速報 556 の `areas.pref` 表記。接尾辞なし）。
pub const KANTO_PREFS_EEW: [&str; 7] = [
    "茨城",
    "栃木",
    "群馬",
    "埼玉",
    "千葉",
    "東京",
    "神奈川",
];

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

/// 通知判定結果。なぜ通知対象になったかの理由も保持する。
#[derive(Debug)]
pub struct NotifyDecision {
    pub notify: bool,
    pub reason: String,
}

/// 通知すべきかを判定する。
///
/// - 関東のいずれかの地点で震度が `kanto_min_scale` 以上 → 通知
/// - もしくは全国の最大震度が `other_min_scale` 以上 → 通知
///
/// 既定では「関東で震度4以上、それ以外は震度5強以上」。
pub fn decide(
    eq: &Earthquake,
    points: &[Point],
    kanto_min_scale: i32,
    other_min_scale: i32,
) -> NotifyDecision {
    let kanto_hit = points.iter().any(|p| {
        KANTO_PREFS.contains(&p.pref.as_str()) && p.scale >= kanto_min_scale
    });

    if kanto_hit {
        let max_kanto = points
            .iter()
            .filter(|p| KANTO_PREFS.contains(&p.pref.as_str()))
            .map(|p| p.scale)
            .max()
            .unwrap_or(-1);
        return NotifyDecision {
            notify: true,
            reason: format!("関東で最大震度{}を観測", scale_label(max_kanto)),
        };
    }

    if eq.max_scale >= other_min_scale {
        return NotifyDecision {
            notify: true,
            reason: format!("全国で最大震度{}を観測", scale_label(eq.max_scale)),
        };
    }

    NotifyDecision {
        notify: false,
        reason: String::new(),
    }
}

/// 緊急地震速報(556)の通知判定。`decide` の予想震度版。
///
/// - 関東いずれかの地域で予想震度（`scale_to`）が `kanto_min_scale` 以上 → 通知
/// - もしくは全国の予想最大震度が `other_min_scale` 以上 → 通知
pub fn decide_eew(areas: &[EewArea], kanto_min_scale: i32, other_min_scale: i32) -> NotifyDecision {
    let max_kanto = areas
        .iter()
        .filter(|a| KANTO_PREFS_EEW.contains(&a.pref.as_str()))
        .map(|a| a.scale_to)
        .max();

    if let Some(max_kanto) = max_kanto {
        if max_kanto >= kanto_min_scale {
            return NotifyDecision {
                notify: true,
                reason: format!("関東で予想最大震度{}", scale_label(max_kanto)),
            };
        }
    }

    let max_all = areas.iter().map(|a| a.scale_to).max().unwrap_or(-1);
    if max_all >= other_min_scale {
        return NotifyDecision {
            notify: true,
            reason: format!("全国で予想最大震度{}", scale_label(max_all)),
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

    #[test]
    fn kanto_shindo4_notifies() {
        let eq = Earthquake {
            max_scale: 40,
            ..Default::default()
        };
        let points = vec![pt("東京都", 40), pt("大阪府", 30)];
        assert!(decide(&eq, &points, 40, 50).notify);
    }

    #[test]
    fn kanto_shindo3_does_not_notify() {
        let eq = Earthquake {
            max_scale: 30,
            ..Default::default()
        };
        let points = vec![pt("千葉県", 30)];
        assert!(!decide(&eq, &points, 40, 50).notify);
    }

    #[test]
    fn other_region_needs_5kyo() {
        let eq = Earthquake {
            max_scale: 40,
            ..Default::default()
        };
        // 関東以外で震度4のみ → 通知しない
        let points = vec![pt("北海道", 40)];
        assert!(!decide(&eq, &points, 40, 50).notify);

        // 関東以外で震度5強 → 通知する
        let eq2 = Earthquake {
            max_scale: 50,
            ..Default::default()
        };
        let points2 = vec![pt("北海道", 50)];
        assert!(decide(&eq2, &points2, 40, 50).notify);
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
        let d = decide_eew(&areas, 40, 50);
        assert!(d.notify);
        assert!(d.reason.contains("関東"));
    }

    #[test]
    fn eew_other_region_needs_5kyo() {
        // 関東外で予想震度4のみ → 通知しない
        assert!(!decide_eew(&[area("北海道", 40)], 40, 50).notify);
        // 関東外で予想震度5強 → 通知する
        assert!(decide_eew(&[area("北海道", 50)], 40, 50).notify);
    }

    #[test]
    fn eew_max_scale_picks_highest() {
        let areas = vec![area("山梨", 45), area("神奈川", 40), area("静岡", 40)];
        assert_eq!(eew_max_scale(&areas), 45);
    }
}
