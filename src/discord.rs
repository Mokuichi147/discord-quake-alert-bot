//! Discord Webhook への通知。embed と地図画像(添付)を送信する。

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::intensity::{embed_color, scale_label, tsunami_label};
use crate::model::JmaQuake;

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

    let magnitude = if eq.hypocenter.magnitude < 0.0 {
        "不明".to_string()
    } else {
        format!("M{:.1}", eq.hypocenter.magnitude)
    };

    let depth = if hypo.depth < 0.0 {
        "不明".to_string()
    } else if hypo.depth == 0.0 {
        "ごく浅い".to_string()
    } else {
        format!("{}km", hypo.depth as i64)
    };

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

    // 出典表示。地図添付時は地理院タイルの出典とURLも明記する（利用規約上必須）。
    let mut footer = String::from("出典: P2P地震情報");
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
