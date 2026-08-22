//! Discord Webhook への通知。embed と地図画像(添付)を送信する。

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tracing::warn;

use crate::intensity::{
    embed_color, eew_area_scale, eew_max_scale, eew_max_scale_label, eew_scale_label,
    has_tsunami, is_unbounded_at, scale_label, tsunami_grade_color, tsunami_grade_label,
    tsunami_grade_rank, tsunami_label,
};
use crate::model::{Eew, JmaQuake, Point, Tsunami};

const MAP_FILE_NAME: &str = "quake.webp";

/// 送信を諦めるまでの試行回数。
///
/// ネットワーク復帰の直後などは最初の1回だけ失敗することがある。そこで通知を落とすと
/// 二度と出ないため（緊急地震速報の第1報は特に取り返しがつかない）、数回だけ試し直す。
const MAX_ATTEMPTS: u32 = 3;
/// 再試行までの最初の待ち時間。
const RETRY_WAIT: Duration = Duration::from_millis(500);
/// 再試行の待ち時間の上限。これより長く待つ必要があるなら通知として手遅れなので諦める。
const RETRY_WAIT_MAX: Duration = Duration::from_secs(5);

/// 受信した地震情報から Discord embed の payload を組み立てる。
///
/// `with_image` が true の場合、地図画像を `attachment://` で参照する。
/// `is_test` が true の場合、テスト送信であることをタイトルとフッターに明示する。
/// `quake.issue` の種別で速報（震度速報）と詳報（各地の震度など）をタイトルで区別する。
pub fn build_payload(quake: &JmaQuake, reason: &str, with_image: bool, is_test: bool) -> Value {
    let eq = &quake.earthquake;
    let hypo = &eq.hypocenter;
    let is_prompt = quake.issue.is_prompt();

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

    // 津波情報がある場合はタイトルに 🌊 を付けて目立たせる。
    let tsunami_mark = if has_tsunami(&eq.domestic_tsunami) {
        "🌊"
    } else {
        ""
    };
    // 速報（震度速報）と詳報（各地の震度など）でタイトルの種別名を変える。
    let kind = if is_prompt { "震度速報" } else { "地震情報" };
    let title = if is_test {
        format!(
            "🧪【テスト通知】{tsunami_mark}{kind}（最大震度 {}）",
            scale_label(eq.max_scale)
        )
    } else {
        format!(
            "🚨{tsunami_mark} {kind}（最大震度 {}）",
            scale_label(eq.max_scale)
        )
    };

    // 速報は震源等が未確定のため、続報で詳細が入る旨を理由文に補足する。
    let description = if is_prompt {
        format!("{reason}（速報のため続報で震源・規模などの詳細が入ります）")
    } else {
        reason.to_string()
    };

    // 出典表示。元データは気象庁（CC BY 4.0）。地図添付時は地理院タイルも明記する。
    let mut footer = String::from("出典: 気象庁（P2P地震情報経由）");
    if with_image {
        footer.push_str(" ・ 地図: 地理院タイル https://maps.gsi.go.jp/development/ichiran.html");
    }
    if is_test {
        footer.push_str(" ・ これはテスト送信です");
    }

    let mut fields = vec![
        json!({ "name": "震源地", "value": place, "inline": true }),
        json!({ "name": "マグニチュード", "value": magnitude, "inline": true }),
        json!({ "name": "深さ", "value": depth, "inline": true }),
        json!({ "name": "発生時刻", "value": time, "inline": false }),
    ];

    // 各地の観測震度がある場合は、震度の高い順に都道府県をまとめて表示する。
    // 震源が未確定な速報段階でも「どこで何の震度を観測したか」を具体的に伝える。
    if let Some(points_text) = fmt_points(&quake.points) {
        fields.push(json!({ "name": "各地の震度", "value": points_text, "inline": false }));
    }

    fields.push(json!({
        "name": "津波",
        "value": tsunami_label(&eq.domestic_tsunami),
        "inline": false,
    }));

    let mut embed = json!({
        "title": title,
        "description": description,
        "color": embed_color(eq.max_scale),
        "fields": fields,
        "footer": { "text": footer },
    });

    if with_image {
        embed["image"] = json!({ "url": format!("attachment://{MAP_FILE_NAME}") });
    }

    json!({ "embeds": [embed] })
}

/// 1つの震度行に並べる名称の上限。超過分は「ほかN{単位}」に畳む。
const MAX_NAMES_PER_SCALE: usize = 12;

/// `(名称, 震度スケール)` の一覧を「震度X: 名称、名称…」形式にまとめる。
/// 震度速報・地震情報・緊急地震速報で震度表記を統一するための共通整形。
///
/// 震度の高い順に並べ、同一震度内は出現順で重複を除き、`MAX_NAMES_PER_SCALE` を
/// 超えたら「ほかN{unit}」に畳む。対象が1つも無ければ `None`。
///
/// `label` は震度スケールを行頭の表記へ変換する。緊急地震速報では「〜程度以上」を
/// 付ける必要があるため、呼び出し側で差し替えられるようにしている。
fn fmt_intensity_groups(
    items: &[(&str, i32)],
    unit: &str,
    label: impl Fn(i32) -> String,
) -> Option<String> {
    use std::collections::BTreeMap;

    // scale -> 名称（出現順・重複なし）。BTreeMap でキー昇順に整列する。
    let mut by_scale: BTreeMap<i32, Vec<&str>> = BTreeMap::new();
    for &(name, scale) in items {
        if scale < 0 || name.is_empty() {
            continue;
        }
        let names = by_scale.entry(scale).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }

    if by_scale.is_empty() {
        return None;
    }

    // 震度の高い順に「X: 名称、名称…」の行を作る（震度はフィールド名で示すため接頭辞は付けない）。
    let lines: Vec<String> = by_scale
        .iter()
        .rev()
        .map(|(scale, names)| {
            let shown = names.len().min(MAX_NAMES_PER_SCALE);
            let mut joined = names[..shown].join("、");
            if names.len() > shown {
                joined.push_str(&format!(" ほか{}{unit}", names.len() - shown));
            }
            format!("{}: {}", label(*scale), joined)
        })
        .collect();

    Some(lines.join("\n"))
}

/// 観測点(`points`)を震度の高い順にまとめ、各震度ごとに観測した地点名（市区町村等）を列挙する。
/// `addr` が空の場合は都道府県名で代用する。観測震度のある点が1つも無ければ `None`（フィールドを出さない）。
fn fmt_points(points: &[Point]) -> Option<String> {
    let items: Vec<(&str, i32)> = points
        .iter()
        .map(|p| {
            let name = if p.addr.is_empty() {
                p.pref.as_str()
            } else {
                p.addr.as_str()
            };
            (name, p.scale)
        })
        .collect();
    fmt_intensity_groups(&items, "地点", |s| scale_label(s).to_string())
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

    // 対象地域を予想震度ごとにまとめる（震度速報・地震情報と同じ「震度X: …」表記に統一）。
    // 「〜程度以上」(99) の地域は下限で分類し、行頭に「程度以上」を付けて下限と分かるようにする。
    // 震度0（揺れを感じない）は「強い揺れが予想される地域」に並べても情報にならない。
    let area_items: Vec<(&str, i32)> = eew
        .areas
        .iter()
        .filter(|a| eew_area_scale(a) > 0)
        .map(|a| (a.name.as_str(), eew_area_scale(a)))
        .collect();
    let area_text = fmt_intensity_groups(&area_items, "地域", |s| {
        eew_scale_label(s, is_unbounded_at(&eew.areas, s))
    })
    .unwrap_or_else(|| "—".to_string());

    let title = format!(
        "{test_prefix}⚡ 緊急地震速報（予想最大震度 {}）",
        eew_max_scale_label(&eew.areas)
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

/// 津波予報(552)から Discord embed の payload を組み立てる。
///
/// 解除報(`cancelled`)の場合は解除の embed を返す。
pub fn build_tsunami_payload(tsunami: &Tsunami, is_test: bool) -> Value {
    let test_prefix = if is_test { "🧪【テスト通知】" } else { "" };

    if tsunami.cancelled {
        let embed = json!({
            "title": format!("{test_prefix}🌊 津波予報 解除"),
            "description": "津波予報（注意報・警報）はすべて解除されました。",
            "color": 0x80_80_80,
            "footer": { "text": "出典: 気象庁 津波予報（P2P地震情報経由）" },
        });
        return json!({ "embeds": [embed] });
    }

    // 最も深刻な grade をタイトル・色に使う。
    let max_grade = tsunami
        .areas
        .iter()
        .max_by_key(|a| tsunami_grade_rank(&a.grade))
        .map(|a| a.grade.as_str())
        .unwrap_or("");

    // grade ごとに対象の津波予報区名をまとめる。
    let mut fields = Vec::new();
    for grade in ["MajorWarning", "Warning", "Watch"] {
        let names: Vec<&str> = tsunami
            .areas
            .iter()
            .filter(|a| a.grade == grade)
            .map(|a| a.name.as_str())
            .collect();
        if !names.is_empty() {
            fields.push(json!({
                "name": tsunami_grade_label(grade),
                "value": names.join("、"),
                "inline": false,
            }));
        }
    }

    // 直ちに来襲のおそれがある場合は強調する。
    let immediate = tsunami.areas.iter().any(|a| a.immediate);
    let description = if immediate {
        "津波予報が発表されています。**直ちに津波来襲のおそれ**があります。沿岸から離れてください。"
    } else {
        "津波予報が発表されています。沿岸では注意してください。"
    };

    let mut footer = String::from("出典: 気象庁 津波予報（P2P地震情報経由）");
    if !tsunami.issue.time.is_empty() {
        footer = format!("{footer} ・ 発表 {}", tsunami.issue.time);
    }

    let embed = json!({
        "title": format!("{test_prefix}🌊 {}", tsunami_grade_label(max_grade)),
        "description": description,
        "color": tsunami_grade_color(max_grade),
        "fields": fields,
        "footer": { "text": footer },
    });

    json!({ "embeds": [embed] })
}

/// `payload` と任意の `image` から、JSON または multipart のリクエストを組み立てる。
///
/// `image` がある場合は multipart で画像を `files[0]` に添付する。payload に
/// `attachments` 等の添付制御フィールドが含まれていればそのまま送られる。
fn build_request(
    builder: reqwest::RequestBuilder,
    payload: &Value,
    image: Option<Vec<u8>>,
) -> Result<reqwest::RequestBuilder> {
    if let Some(bytes) = image {
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(MAP_FILE_NAME)
            .mime_str("image/webp")?;
        let form = reqwest::multipart::Form::new()
            .text("payload_json", serde_json::to_string(payload)?)
            .part("files[0]", part);
        Ok(builder.multipart(form))
    } else {
        Ok(builder.json(payload))
    }
}

/// レート制限時に Discord が指示してくる待ち時間（`Retry-After`、秒）。
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let raw = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: f64 = raw.trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// リクエストを送信し、一時的な失敗だけ試し直して成功時の本文を返す。
///
/// 再試行するのは接続エラー・タイムアウトと、429（レート制限）・5xx のみ。
/// それ以外の 4xx は投げ直しても結果が変わらないため即座に諦める。
/// `new_request` は試行ごとにリクエストを組み直すために毎回呼ばれる。
async fn send_with_retry(
    new_request: impl Fn() -> reqwest::RequestBuilder,
    payload: &Value,
    image: Option<Vec<u8>>,
    what: &'static str,
) -> Result<String> {
    let mut attempt: u32 = 1;
    let mut wait = RETRY_WAIT;
    loop {
        let last = attempt >= MAX_ATTEMPTS;
        let request = build_request(new_request(), payload, image.clone())?;
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let after = retry_after(&response);
                let body = response.text().await.unwrap_or_default();
                if status.is_success() {
                    return Ok(body);
                }
                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                if !retryable || last {
                    anyhow::bail!("{what}がエラー応答: {status} {body}");
                }
                if let Some(after) = after {
                    if after > RETRY_WAIT_MAX {
                        anyhow::bail!("{what}がレート制限: {after:?} の待機が必要なため諦めます");
                    }
                    wait = after;
                }
                warn!(%status, attempt, wait_ms = wait.as_millis() as u64, "{what}に失敗。再試行します");
            }
            Err(e) if last => return Err(anyhow::Error::new(e).context(what)),
            Err(e) => {
                warn!(error = %e, attempt, wait_ms = wait.as_millis() as u64, "{what}に失敗。再試行します");
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 3).min(RETRY_WAIT_MAX);
        attempt += 1;
    }
}

/// Webhook に送信する。`image` がある場合は multipart で画像を添付する。
///
/// message_id を必要としない通知（緊急地震速報の取消・津波予報）向け。
pub async fn send(
    client: &reqwest::Client,
    webhook_url: &str,
    payload: &Value,
    image: Option<Vec<u8>>,
) -> Result<()> {
    send_with_retry(|| client.post(webhook_url), payload, image, "Webhook 送信").await?;
    Ok(())
}

/// Webhook に新規投稿し、作成されたメッセージの ID を返す。
///
/// 後で編集（差し替え）できるよう `?wait=true` を付けてレスポンスから ID を取得する。
pub async fn post_message(
    client: &reqwest::Client,
    webhook_url: &str,
    payload: &Value,
    image: Option<Vec<u8>>,
) -> Result<String> {
    let url = format!("{webhook_url}?wait=true");
    let body = send_with_retry(|| client.post(&url), payload, image, "Webhook 投稿").await?;
    let value: Value = serde_json::from_str(&body).context("Webhook 応答の解析に失敗")?;
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("Webhook 応答に message id がありません")
}

/// 既存の Webhook メッセージを編集（差し替え）する。
///
/// 画像ありの場合は `files[0]` を再アップロードし、`attachments` で旧添付を置き換える。
/// 画像なしの場合は `attachments` を空配列にして旧添付を取り除く。
pub async fn edit_message(
    client: &reqwest::Client,
    webhook_url: &str,
    message_id: &str,
    payload: &Value,
    image: Option<Vec<u8>>,
) -> Result<()> {
    let url = format!("{webhook_url}/messages/{message_id}");

    // 添付の置き換え指示を payload に付与する。
    let mut payload = payload.clone();
    let attachments = if image.is_some() {
        json!([{ "id": 0, "filename": MAP_FILE_NAME }])
    } else {
        json!([])
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("attachments".to_string(), attachments);
    }

    send_with_retry(|| client.patch(&url), &payload, image, "Webhook 編集").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(pref: &str, addr: &str, scale: i32) -> Point {
        Point {
            pref: pref.to_string(),
            addr: addr.to_string(),
            scale,
        }
    }

    #[test]
    fn points_grouped_by_scale_desc() {
        let points = vec![
            pt("宮城県", "宮城県北部", 45),
            pt("福島県", "福島県中通り", 40),
            pt("宮城県", "宮城県北部", 45), // 重複は畳む
            pt("岩手県", "岩手県沿岸南部", 40),
        ];
        let text = fmt_points(&points).expect("観測点があるので Some");
        assert_eq!(text, "5弱: 宮城県北部\n4: 福島県中通り、岩手県沿岸南部");
    }

    #[test]
    fn points_falls_back_to_pref_when_addr_empty() {
        let points = vec![pt("青森県", "", 40), pt("岩手県", "", 40)];
        let text = fmt_points(&points).expect("観測点があるので Some");
        assert_eq!(text, "4: 青森県、岩手県");
    }

    #[test]
    fn points_empty_returns_none() {
        assert!(fmt_points(&[]).is_none());
        // 震度不明(-1)や地点名なしは対象外。
        assert!(fmt_points(&[pt("", "", 45), pt("宮城県", "宮城県北部", -1)]).is_none());
    }

    #[test]
    fn points_over_limit_are_folded() {
        let points: Vec<Point> = (0..MAX_NAMES_PER_SCALE + 3)
            .map(|i| pt(&format!("県{i}"), &format!("地点{i}"), 40))
            .collect();
        let text = fmt_points(&points).unwrap();
        assert!(text.contains("ほか3地点"), "超過分が畳まれる: {text}");
    }

    #[test]
    fn eew_areas_use_same_intensity_notation() {
        // 緊急地震速報の対象地域も震度速報と同じ「震度X: …」表記に統一する。
        let items = vec![
            ("神奈川県西部", 45),
            ("東京都23区", 40),
            ("神奈川県東部", 45),
        ];
        let text = fmt_intensity_groups(&items, "地域", |s| scale_label(s).to_string()).unwrap();
        assert_eq!(text, "5弱: 神奈川県西部、神奈川県東部\n4: 東京都23区");
    }

    #[test]
    fn eew_area_list_omits_shindo0() {
        // 震度0の地域は「強い揺れが予想される地域」に載せない。
        let eew = Eew {
            code: 556,
            cancelled: false,
            issue: Default::default(),
            earthquake: Default::default(),
            areas: vec![
                crate::model::EewArea {
                    pref: "神奈川".to_string(),
                    name: "神奈川県西部".to_string(),
                    scale_from: 45,
                    scale_to: 45,
                },
                crate::model::EewArea {
                    pref: "東京".to_string(),
                    name: "東京都23区".to_string(),
                    scale_from: 0,
                    scale_to: 0,
                },
            ],
        };
        let payload = build_eew_payload(&eew, "", false, false);
        let areas = payload["embeds"][0]["fields"][4]["value"].as_str().unwrap();
        assert_eq!(areas, "5弱: 神奈川県西部");
    }
}
