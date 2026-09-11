//! Discord Webhook への通知。embed と地図画像(添付)を送信する。

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::StatusCode;
use serde_json::{json, Value};
use tracing::warn;

use crate::config::WatchedPoint;
use crate::intensity::{
    eew_area_scale, eew_max_scale, eew_max_scale_label, eew_scale_label, embed_color, has_tsunami,
    is_unbounded_at, normalize_pref, scale_label, tsunami_grade_color, tsunami_grade_label,
    tsunami_grade_rank, tsunami_label,
};
use crate::model::{Eew, JmaQuake, Point, Tsunami};

const MAP_FILE_NAME: &str = "quake.webp";

/// 登録名は都道府県と地域・観測点名の完全一致で照合する。
/// 周辺地域の値から登録地点の震度を推測しない。
fn matches_watched(point: &WatchedPoint, pref: &str, name: &str) -> bool {
    normalize_pref(&point.pref) == normalize_pref(pref) && point.name == name
}

/// 任意の表示名に含まれる Discord の Markdown 記号を無効化する。
fn escape_markdown(value: &str) -> String {
    let mut escaped = String::new();
    for c in value.chars() {
        if "\\`*_~|>[]()#".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn watched_line(point: &WatchedPoint, intensity: &str) -> String {
    let name = escape_markdown(&point.name);
    let pref = escape_markdown(&point.pref);
    let place = if point.label.is_empty() {
        format!("{name}（{pref}）")
    } else {
        format!("{}（{pref}・{name}）", escape_markdown(&point.label))
    };
    format!("**{place}：震度{intensity}**")
}

/// 件数にかかわらず Discord のフィールド長上限に収め、超過件数を明示する。
fn prepend_watched_field(payload: &mut Value, title: &str, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    let mut value = String::new();
    for (i, line) in lines.iter().enumerate() {
        // 残り件数の表示用に余白を確保する。UTF-16で数え、絵文字も安全に扱う。
        if value.encode_utf16().count() + line.encode_utf16().count() + 1 > 950 {
            value.push_str(&format!("\nほか{}地点（表示上限）", lines.len() - i));
            break;
        }
        if !value.is_empty() {
            value.push('\n');
        }
        value.push_str(line);
    }
    if let Some(fields) = payload["embeds"][0]["fields"].as_array_mut() {
        fields.insert(0, json!({"name": title, "value": value, "inline": false}));
    }
}

/// 情報源に含まれる登録地点の観測震度を先頭に表示する。
pub fn highlight_watched_quake(payload: &mut Value, quake: &JmaQuake, watched: &[WatchedPoint]) {
    let lines = watched
        .iter()
        .filter_map(|point| {
            quake
                .points
                .iter()
                .filter(|p| {
                    matches_watched(point, &p.pref, &p.addr) && scale_label(p.scale) != "不明"
                })
                .max_by_key(|p| p.scale)
                .map(|p| watched_line(point, scale_label(p.scale)))
        })
        .collect();
    prepend_watched_field(payload, "📍 登録地点の震度", lines);
}

/// 情報源に含まれる登録地域の予想震度を先頭に表示する。取消報には表示しない。
pub fn highlight_watched_eew(payload: &mut Value, eew: &Eew, watched: &[WatchedPoint]) {
    if eew.cancelled {
        return;
    }
    let lines = watched
        .iter()
        .filter_map(|point| {
            eew.areas
                .iter()
                .filter(|a| {
                    matches_watched(point, &a.pref, &a.name)
                        && scale_label(eew_area_scale(a)) != "不明"
                })
                .max_by_key(|a| (a.scale_to == 99, eew_area_scale(a)))
                .map(|a| {
                    let upper = eew_scale_label(eew_area_scale(a), a.scale_to == 99);
                    let intensity = if a.scale_to != 99
                        && a.scale_from < a.scale_to
                        && scale_label(a.scale_from) != "不明"
                    {
                        format!("{}〜{upper}", scale_label(a.scale_from))
                    } else {
                        upper
                    };
                    watched_line(point, &intensity)
                })
        })
        .collect();
    prepend_watched_field(payload, "📍 登録地点の予想震度", lines);
}

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
    let kind = if is_prompt {
        "震度速報"
    } else {
        "地震情報"
    };
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
        footer.push_str(
            " ・ 観測点座標: 気象庁 https://ds.data.jma.go.jp/eqev/data/intens-st/ を本botで加工",
        );
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
    let test_prefix = if is_test {
        "🧪【テスト通知】"
    } else {
        ""
    };

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
        footer.push_str(
            " ・ 観測点座標: 気象庁 https://ds.data.jma.go.jp/eqev/data/intens-st/ を本botで加工",
        );
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
    let test_prefix = if is_test {
        "🧪【テスト通知】"
    } else {
        ""
    };

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

/// 失敗した送信を、呼び出し側が投げ直してよいか。
///
/// エラーに付けて返す。投稿の作成が「届いたか分からない」形で失敗した場合だけ
/// `Unsafe` になり、それ以外は投げ直しても重複しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryable {
    /// 投げ直してよい。届いていないと分かっているか、何度投げても結果が同じ。
    Safe,
    /// 投げ直すと二重投稿になりうる。Discord 側では受理済みかもしれない。
    Unsafe,
}

impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Safe => write!(f, "再送可"),
            Self::Unsafe => write!(f, "再送不可（二重投稿のおそれ）"),
        }
    }
}

/// この失敗を投げ直してよいか。判断が付かないものは投げ直してよい扱いにする
/// （送信まで到達していない失敗なので、重複は起きない）。
pub fn retry_is_safe(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Retryable>() != Some(&Retryable::Unsafe)
}

/// 再試行の方針。同じ要求を投げ直してよいかどうかで分ける。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Retry {
    /// 何度投げても結果が同じ要求（メッセージの編集）。曖昧な失敗でも試し直す。
    Idempotent,
    /// メッセージを作る要求。届いたか分からない失敗で投げ直すと二重投稿になり、
    /// しかも記録できる message_id は後から作られた方だけなので、先に作られた
    /// メッセージは続報で差し替えられなくなる。確実に届いていない場合だけ試し直す。
    CreateOnce,
}

/// 曖昧な失敗かどうかから、呼び出し側が投げ直してよいかを決める。
fn classify(ambiguous: bool) -> Retryable {
    if ambiguous {
        Retryable::Unsafe
    } else {
        Retryable::Safe
    }
}

/// リクエストを送信し、一時的な失敗だけ試し直して成功時の本文を返す。
///
/// 再試行するのは接続エラー・タイムアウトと、429（レート制限）・5xx のみ。
/// それ以外の 4xx は投げ直しても結果が変わらないため即座に諦める。
/// ただし `Retry::CreateOnce` では、届いたかどうかが曖昧な失敗（タイムアウトや
/// 5xx。Discord 側では受理済みかもしれない）では試し直さない。取りこぼしても、
/// 緊急地震速報は数秒後の続報が新規投稿されるし、地震情報と津波予報は再接続時に
/// 履歴から拾い直されるため、二重投稿を作るより落とす方を選ぶ。
/// `new_request` は試行ごとにリクエストを組み直すために毎回呼ばれる。
async fn send_with_retry(
    new_request: impl Fn() -> reqwest::RequestBuilder,
    payload: &Value,
    image: Option<Vec<u8>>,
    what: &'static str,
    retry: Retry,
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
                // 429 は要求が実行されずに弾かれた印なので、投稿の作成でも試し直せる。
                let retryable = status == StatusCode::TOO_MANY_REQUESTS
                    || (status.is_server_error() && retry == Retry::Idempotent);
                if !retryable || last {
                    // 5xx は Discord 側で受理済みかもしれない。投稿の作成なら、
                    // 呼び出し側にも投げ直させない。
                    let ambiguous = status.is_server_error() && retry == Retry::CreateOnce;
                    return Err(anyhow!("{what}がエラー応答: {status} {body}"))
                        .context(classify(ambiguous));
                }
                if let Some(after) = after {
                    if after > RETRY_WAIT_MAX {
                        // 429 は弾かれているので、投げ直しても重複はしない。
                        return Err(anyhow!(
                            "{what}がレート制限: {after:?} の待機が必要なため諦めます"
                        ))
                        .context(Retryable::Safe);
                    }
                    wait = after;
                }
                warn!(%status, attempt, wait_ms = wait.as_millis() as u64, "{what}に失敗。再試行します");
            }
            // 接続できなかった要求は届いていないので、投稿の作成でも試し直せる。
            Err(e) if last || (retry == Retry::CreateOnce && !e.is_connect()) => {
                // タイムアウトなどは Discord 側で受理済みかもしれない。
                let ambiguous = retry == Retry::CreateOnce && !e.is_connect();
                return Err(anyhow::Error::new(e))
                    .context(what)
                    .context(classify(ambiguous));
            }
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
    send_with_retry(
        || client.post(webhook_url),
        payload,
        image,
        "Webhook 送信",
        Retry::CreateOnce,
    )
    .await?;
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
    let body = send_with_retry(
        || client.post(&url),
        payload,
        image,
        "Webhook 投稿",
        Retry::CreateOnce,
    )
    .await?;

    // ここまで来たということは 2xx が返っており、投稿は作られている。以降の失敗は
    // message_id が取れないだけなので、投げ直させない（投げ直すと二重投稿になる）。
    let value: Value = serde_json::from_str(&body)
        .context("Webhook 応答の解析に失敗")
        .context(Retryable::Unsafe)?;
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("Webhook 応答に message id がありません")
        .context(Retryable::Unsafe)
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

    send_with_retry(
        || client.patch(&url),
        &payload,
        image,
        "Webhook 編集",
        Retry::Idempotent,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(pref: &str, name: &str, label: &str) -> WatchedPoint {
        WatchedPoint {
            pref: pref.into(),
            name: name.into(),
            label: label.into(),
        }
    }

    #[test]
    fn watched_quake_matches_exact_location_and_updates_payload() {
        let mut quake: JmaQuake = serde_json::from_value(json!({
            "code":551, "points":[
                {"pref":"東京都","addr":"東京都23区","scale":30},
                {"pref":"東京都","addr":"東京都23区","scale":40},
                {"pref":"東京都","addr":"不明地点","scale":-1}
            ]
        }))
        .unwrap();
        let watched = vec![
            watched("東京", "東京都23区", "自宅周辺"),
            watched("千葉県", "東京都23区", "同名でも別の県"),
            watched("東京都", "東京都23", "部分一致しない"),
            watched("東京都", "不明地点", "不明"),
        ];
        let base = build_payload(&quake, "通知理由", false, false);
        let mut payload = base.clone();
        highlight_watched_quake(&mut payload, &quake, &watched);
        assert_eq!(
            payload["embeds"][0]["fields"][0]["name"],
            "📍 登録地点の震度"
        );
        assert_eq!(
            payload["embeds"][0]["fields"][0]["value"],
            "**自宅周辺（東京・東京都23区）：震度4**"
        );
        let mut unchanged = base.clone();
        highlight_watched_quake(&mut unchanged, &quake, &[]);
        assert_eq!(unchanged, base);
        highlight_watched_quake(&mut unchanged, &quake, &watched[1..]);
        assert_eq!(unchanged, base);
        quake.points[1].scale = 45;
        let mut revised = base;
        highlight_watched_quake(&mut revised, &quake, &watched);
        assert_ne!(payload, revised);
    }

    #[test]
    fn watched_eew_preserves_forecast_range_and_unbounded_scale() {
        let mut eew: Eew = serde_json::from_value(json!({
            "code":556,"areas":[
                {"pref":"東京","name":"東京都23区","scaleFrom":40,"scaleTo":50},
                {"pref":"神奈川","name":"神奈川県東部","scaleFrom":45,"scaleTo":99}
            ]
        }))
        .unwrap();
        let watched = vec![
            watched("東京都", "東京都23区", "自宅周辺"),
            watched("神奈川県", "神奈川県東部", "職場周辺"),
        ];
        let mut payload = build_eew_payload(&eew, "", false, false);
        highlight_watched_eew(&mut payload, &eew, &watched);
        let field = &payload["embeds"][0]["fields"][0];
        assert_eq!(field["name"], "📍 登録地点の予想震度");
        let value = field["value"].as_str().unwrap();
        assert!(value.contains("震度4〜5強"));
        assert!(value.contains("震度5弱程度以上"));
        eew.cancelled = true;
        let original = build_eew_payload(&eew, "", false, false);
        let mut cancelled = original.clone();
        highlight_watched_eew(&mut cancelled, &eew, &watched);
        assert_eq!(cancelled, original);
    }

    #[test]
    fn many_watched_points_fit_field_limit() {
        let mut payload = json!({"embeds":[{"fields":[]}]});
        let lines = (0..100)
            .map(|i| {
                watched_line(
                    &watched("東京都", "東京都23区", &format!("地点{i}🏠")),
                    "5強",
                )
            })
            .collect();
        prepend_watched_field(&mut payload, "📍 登録地点の震度", lines);
        let value = payload["embeds"][0]["fields"][0]["value"].as_str().unwrap();
        assert!(value.encode_utf16().count() <= 1024);
        assert!(value.contains("地点0"));
        assert!(value.ends_with("地点（表示上限）"));
        assert!(
            watched_line(&watched("東京都", "東京都23区", "*自宅*"), "4").contains("\\*自宅\\*")
        );
    }

    /// プロキシの自動検出は macOS で panic することがあるため無効にする。
    fn test_client(timeout: Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(timeout)
            .build()
            .unwrap()
    }

    #[test]
    fn retry_is_safe_unless_the_post_may_have_landed() {
        assert!(!retry_is_safe(&anyhow!("失敗").context(Retryable::Unsafe)));
        assert!(retry_is_safe(&anyhow!("失敗").context(Retryable::Safe)));
        // 送信まで到達していない失敗には印が付かない。投げ直しても重複しない。
        assert!(retry_is_safe(&anyhow!("印の無い失敗")));
    }

    #[tokio::test]
    async fn post_that_may_have_landed_is_not_safe_to_retry() {
        // 応答が返らない＝Discord 側で受理済みかもしれない。投げ直すと二重投稿。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _accepted = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let http = test_client(Duration::from_millis(200));
        let error = post_message(&http, &format!("http://{addr}/hook"), &json!({}), None)
            .await
            .unwrap_err();
        assert!(!retry_is_safe(&error));
    }

    /// 1回だけ 200 と `body` を返すサーバを立て、その URL を返す。
    async fn serve_once(body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            // 要求を読んでから応答する。読まずに閉じると相手の書き込みが失敗する。
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
        format!("http://{addr}/hook")
    }

    #[tokio::test]
    async fn post_with_an_unreadable_response_is_not_safe_to_retry() {
        // 2xx が返っている＝投稿は作られている。応答の解析に失敗しただけなので、
        // 投げ直すと二重投稿になる。
        let http = test_client(Duration::from_secs(5));

        let url = serve_once("これは JSON ではない").await;
        let error = post_message(&http, &url, &json!({}), None)
            .await
            .unwrap_err();
        assert!(!retry_is_safe(&error), "解析に失敗しても投げ直さない");

        // message id が無い応答も同じ。
        let url = serve_once("{}").await;
        let error = post_message(&http, &url, &json!({}), None)
            .await
            .unwrap_err();
        assert!(!retry_is_safe(&error), "message id が無くても投げ直さない");
    }

    #[tokio::test]
    async fn post_that_never_connected_is_safe_to_retry() {
        // 接続できていないので、投げ直しても重複しない。
        let http = test_client(Duration::from_millis(200));
        let error = post_message(&http, "http://127.0.0.1:1/hook", &json!({}), None)
            .await
            .unwrap_err();
        assert!(retry_is_safe(&error));
    }

    fn pt(pref: &str, addr: &str, scale: i32) -> Point {
        Point {
            pref: pref.to_string(),
            addr: addr.to_string(),
            is_area: false,
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
