//! Discord Webhook への通知。embed と地図画像(添付)を送信する。

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::intensity::{embed_color, eew_max_scale, scale_label, tsunami_label};
use crate::model::{Eew, JmaQuake};

const MAP_FILE_NAME: &str = "quake.webp";

/// 受信した地震情報から Discord embed の payload を組み立てる。
///
/// `with_image` が true の場合、地図画像を `attachment://` で参照する。
/// `is_test` が true の場合、テスト送信であることをタイトルとフッターに明示する。
pub fn build_payload(quake: &JmaQuake, reason: &str, with_image: bool, is_test: bool) -> Value {
    let eq = &quake.earthquake;
    let hypo = &eq.hypocenter;

    let place = if hypo.name.is_empty() {
        "不明".to_string()
    } else {
        hypo.name.clone()
    };

    let magnitude = fmt_magnitude(eq.hypocenter.magnitude);
    let depth = fmt_depth(hypo.depth);

    let time = if eq.time.is_empty() {
        "不明".to_string()
    } else {
        eq.time.clone()
    };

    let title = if is_test {
        format!("🧪【テスト通知】地震情報（最大震度 {}）", scale_label(eq.max_scale))
    } else {
        format!("🚨 地震情報（最大震度 {}）", scale_label(eq.max_scale))
    };

    // 出典表示。元データは気象庁（CC BY 4.0）。地図添付時は地理院タイルも明記する。
    let mut footer = String::from("出典: 気象庁（P2P地震情報経由）");
    if with_image {
        footer.push_str(" ・ 地図: 地理院タイル https://maps.gsi.go.jp/development/ichiran.html");
    }
    if is_test {
        footer.push_str(" ・ これはテスト送信です");
    }

    let mut embed = json!({
        "title": title,
        "description": reason,
        "color": embed_color(eq.max_scale),
        "fields": [
            { "name": "震源地", "value": place, "inline": true },
            { "name": "マグニチュード", "value": magnitude, "inline": true },
            { "name": "深さ", "value": depth, "inline": true },
            { "name": "発生時刻", "value": time, "inline": false },
            { "name": "津波", "value": tsunami_label(&eq.domestic_tsunami), "inline": false },
        ],
        "footer": { "text": footer },
    });

    if with_image {
        embed["image"] = json!({ "url": format!("attachment://{MAP_FILE_NAME}") });
    }

    json!({ "embeds": [embed] })
}

/// マグニチュード表記（不明は -1 未満で判定）。
fn fmt_magnitude(magnitude: f64) -> String {
    if magnitude < 0.0 {
        "不明".to_string()
    } else {
        format!("M{magnitude:.1}")
    }
}

/// 深さ表記（不明=負値、0=ごく浅い）。
fn fmt_depth(depth: f64) -> String {
    if depth < 0.0 {
        "不明".to_string()
    } else if depth == 0.0 {
        "ごく浅い".to_string()
    } else {
        format!("{}km", depth as i64)
    }
}

/// 緊急地震速報(556)から Discord embed の payload を組み立てる。
///
/// 取消報(`cancelled`)の場合は取消の embed を返す。
pub fn build_eew_payload(eew: &Eew, reason: &str, with_image: bool, is_test: bool) -> Value {
    let test_prefix = if is_test { "🧪【テスト通知】" } else { "" };

    if eew.cancelled {
        let embed = json!({
            "title": format!("{test_prefix}⚠️ 緊急地震速報 取消"),
            "description": "先ほどの緊急地震速報は取り消されました。",
            "color": 0x80_80_80,
            "footer": { "text": "出典: 気象庁 緊急地震速報（P2P地震情報経由）" },
        });
        return json!({ "embeds": [embed] });
    }

    let hypo = &eew.earthquake.hypocenter;
    let max_scale = eew_max_scale(&eew.areas);

    let place = if hypo.name.is_empty() {
        "不明".to_string()
    } else {
        hypo.name.clone()
    };
    let time = if eew.issue.time.is_empty() {
        "不明".to_string()
    } else {
        eew.issue.time.clone()
    };

    // 予想震度が高い順に対象地域名を列挙（最大8件）。
    let mut areas: Vec<_> = eew.areas.iter().collect();
    areas.sort_by(|a, b| b.scale_to.cmp(&a.scale_to));
    let area_text = if areas.is_empty() {
        "—".to_string()
    } else {
        let mut names: Vec<String> = areas
            .iter()
            .take(8)
            .map(|a| format!("{}（{}）", a.name, scale_label(a.scale_to)))
            .collect();
        if areas.len() > 8 {
            names.push(format!("ほか{}地域", areas.len() - 8));
        }
        names.join("\n")
    };

    let title = format!(
        "{test_prefix}⚡ 緊急地震速報（予想最大震度 {}）",
        scale_label(max_scale)
    );

    let mut footer = String::from("出典: 気象庁 緊急地震速報（P2P地震情報経由・予想値）");
    if with_image {
        footer.push_str(" ・ 地図: 地理院タイル https://maps.gsi.go.jp/development/ichiran.html");
    }

    let mut embed = json!({
        "title": title,
        "description": format!("{reason}（速報・予想値のため続報で変わることがあります）"),
        "color": embed_color(max_scale),
        "fields": [
            { "name": "震源地", "value": place, "inline": true },
            { "name": "マグニチュード", "value": fmt_magnitude(hypo.magnitude), "inline": true },
            { "name": "深さ", "value": fmt_depth(hypo.depth), "inline": true },
            { "name": "発表時刻", "value": time, "inline": false },
            { "name": "強い揺れが予想される地域", "value": area_text, "inline": false },
        ],
        "footer": { "text": footer },
    });

    if with_image {
        embed["image"] = json!({ "url": format!("attachment://{MAP_FILE_NAME}") });
    }

    json!({ "embeds": [embed] })
}

/// Webhook に送信する。`image` がある場合は multipart で画像を添付する。
pub async fn send(
    client: &reqwest::Client,
    webhook_url: &str,
    payload: &Value,
    image: Option<Vec<u8>>,
) -> Result<()> {
    let response = if let Some(bytes) = image {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(MAP_FILE_NAME)
            .mime_str("image/webp")?;

        let form = reqwest::multipart::Form::new()
            .text("payload_json", serde_json::to_string(payload)?)
            .part("files[0]", part);

        client
            .post(webhook_url)
            .multipart(form)
            .send()
            .await
            .context("Webhook(multipart)送信に失敗")?
    } else {
        client
            .post(webhook_url)
            .json(payload)
            .send()
            .await
            .context("Webhook(json)送信に失敗")?
    };

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Webhook がエラー応答: {status} {body}");
    }
    Ok(())
}
