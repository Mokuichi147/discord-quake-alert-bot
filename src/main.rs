//! 地震bot エントリポイント。
//!
//! P2P地震情報の WebSocket を購読し、日本国内の強い揺れの地震を
//! 地図画像付きで Discord Webhook へ通知する。

mod config;
mod discord;
mod geo;
mod intensity;
mod mapgen;
mod model;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::intensity::{decide, decide_eew, eew_max_scale, tsunami_grade_rank};
use crate::model::{Eew, Envelope, JmaQuake, Tsunami};

/// 地震情報メッセージの code。
const CODE_JMA_QUAKE: i32 = 551;
/// 緊急地震速報（警報）メッセージの code。
const CODE_EEW: i32 = 556;
/// 津波予報メッセージの code。
const CODE_TSUNAMI: i32 = 552;

/// 重複抑制のため記録する ID の上限。
const SEEN_ID_CAPACITY: usize = 256;

/// 通知済みの ID を保持し、重複・続報を抑制する汎用の記録。
#[derive(Default)]
struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenIds {
    /// 未登録なら記録して true を返す。既に登録済みなら false。
    fn mark_if_new(&mut self, id: &str) -> bool {
        if !self.set.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > SEEN_ID_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }

    /// 既に登録済みか。
    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
}

/// 地震情報(551)の1報あたりの投稿状態。差し替え用に message_id と内容ハッシュを保持する。
struct QuakePost {
    /// 表示内容のハッシュ。一致すれば再投稿しない。
    signature: u64,
    /// 投稿済み Discord メッセージの ID。内容変更時はこれを編集する。
    message_id: String,
}

/// 同一地震（発生時刻キー）について、速報・詳報それぞれの投稿状態を別管理する。
#[derive(Default)]
struct QuakeEntry {
    /// 震度速報（ScalePrompt）の投稿状態。
    prompt: Option<QuakePost>,
    /// 詳報（各地の震度など）の投稿状態。
    detail: Option<QuakePost>,
}

/// 地震情報(551)の投稿状態を発生時刻ごとに保持する。容量超過で古い順に退避する。
#[derive(Default)]
struct QuakeTracker {
    map: HashMap<String, QuakeEntry>,
    order: VecDeque<String>,
}

impl QuakeTracker {
    /// 発生時刻キーのエントリを取得（無ければ作成）する。
    fn entry(&mut self, key: &str) -> &mut QuakeEntry {
        if !self.map.contains_key(key) {
            self.map.insert(key.to_string(), QuakeEntry::default());
            self.order.push_back(key.to_string());
            if self.order.len() > SEEN_ID_CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        self.map.get_mut(key).expect("直前に挿入済み")
    }
}

/// 表示用 payload から内容シグネチャ（ハッシュ）を求める。表示フィールドが全て反映される。
fn signature_of(payload: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    payload.to_string().hash(&mut hasher);
    hasher.finish()
}

/// 種別ごとの重複抑制状態。
#[derive(Default)]
struct DedupState {
    /// 緊急地震速報の eventId（第1報のみ通知）。
    eews: SeenIds,
    /// 津波予報の id（同一発表の再送を除去）。
    tsunamis: SeenIds,
    /// 地震情報(551)の投稿状態（速報・詳報を発生時刻ごとに保持）。
    quakes: QuakeTracker,
}

#[tokio::main]
async fn main() -> Result<()> {
    // .env があれば読み込む（無ければ無視）。
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    info!(
        ws_url = %config.ws_url,
        regions = config.region_min_scales.len(),
        other_min = config.other_min_scale,
        attach_map = config.attach_map,
        "地震botを起動しました"
    );

    let http = reqwest::Client::builder()
        .user_agent("quake-alert-bot/0.1 (+https://github.com/)")
        .timeout(Duration::from_secs(30))
        .build()?;

    // テストモード: 過去のデータを1件取得して送信し終了する。
    if std::env::args().any(|a| a == "--test-tsunami") {
        return run_test_tsunami(&config, &http).await;
    }
    if std::env::args().any(|a| a == "--test-eew") {
        return run_test_eew(&config, &http).await;
    }
    if std::env::args().any(|a| a == "--test-prompt") {
        return run_test_prompt(&config, &http).await;
    }
    if std::env::args().any(|a| a == "--test") {
        return run_test(&config, &http).await;
    }

    // 重複報・再送を抑制する状態。再接続をまたいで保持する。
    let mut dedup = DedupState::default();

    // 切断されても再接続し続ける。失敗時は指数バックオフ（最大60秒）。
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_once(&config, &http, &mut dedup).await {
            Ok(()) => {
                warn!("WebSocket 接続が終了しました。再接続します");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                error!(error = %e, backoff_secs = backoff.as_secs(), "接続エラー。再接続します");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// 1回の WebSocket セッションを処理する。正常切断で Ok を返す。
async fn run_once(config: &Config, http: &reqwest::Client, dedup: &mut DedupState) -> Result<()> {
    let (ws_stream, _resp) = tokio_tungstenite::connect_async(config.ws_url.as_str()).await?;
    info!("WebSocket に接続しました");
    let (_write, mut read) = ws_stream.split();

    while let Some(message) = read.next().await {
        let message = message?;
        match message {
            Message::Text(text) => {
                if let Err(e) = handle_text(config, http, &text, false, dedup).await {
                    error!(error = %e, "メッセージ処理に失敗");
                }
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => {
                info!("サーバから切断通知を受信");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// 過去の地震情報 (P2P地震情報 REST API) のエンドポイント。
const HISTORY_URL: &str = "https://api.p2pquake.net/v2/history?codes=551&limit=50";
/// 過去の緊急地震速報 (556) のエンドポイント。
const HISTORY_EEW_URL: &str = "https://api.p2pquake.net/v2/history?codes=556&limit=50";
/// 過去の津波予報 (552) のエンドポイント。
const HISTORY_TSUNAMI_URL: &str = "https://api.p2pquake.net/v2/history?codes=552&limit=50";

/// テスト用: 過去の地震情報から通知条件を満たす最新の1件を選び、
/// 本番と同じ経路 (handle_text) で Discord へ送信して終了する。
async fn run_test(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の地震情報を取得します");
    let body = http
        .get(HISTORY_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body)?;
    info!(count = items.len(), "履歴を取得しました");

    // 通知条件を満たす最新の地震を1件だけ送信する。
    for item in &items {
        let text = item.to_string();
        let quake: JmaQuake = match serde_json::from_str(&text) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let decision = decide(
            &quake.earthquake,
            &quake.points,
            &config.region_min_scales,
            config.other_min_scale,
        );
        if decision.notify {
            info!(
                place = %quake.earthquake.hypocenter.name,
                max_scale = quake.earthquake.max_scale,
                time = %quake.earthquake.time,
                "テスト送信する地震を選択しました"
            );
            handle_text(config, http, &text, true, &mut DedupState::default()).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に通知条件を満たす地震がありませんでした。しきい値を下げて再試行してください");
    Ok(())
}

/// テスト用: 過去の地震情報から「震源未確定（震度速報など）」かつ観測県があり、
/// 通知条件を満たす最新の1件を選び、本番と同じ経路 (handle_quake) で送信して終了する。
///
/// 通常の `--test` は最新の通知対象（多くは震源確定済みの詳報）を選ぶため、震源未確定時に
/// 使う観測県マーカーマップの経路を確認できない。本コマンドはその経路を狙って検証する。
async fn run_test_prompt(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の地震情報から震源未確定の報を取得します");
    let body = http
        .get(HISTORY_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body)?;
    info!(count = items.len(), "履歴を取得しました");

    for item in &items {
        let text = item.to_string();
        let quake: JmaQuake = match serde_json::from_str(&text) {
            Ok(q) => q,
            Err(_) => continue,
        };
        // 震源座標が有効な報は震源マップ経路（=通常の --test で確認できる）なので除外。
        if quake.earthquake.hypocenter.has_valid_coords() {
            continue;
        }
        // 観測県マーカーが1つも作れない報も対象外。
        if geo::points_to_markers(&quake.points).is_empty() {
            continue;
        }
        let decision = decide(
            &quake.earthquake,
            &quake.points,
            &config.region_min_scales,
            config.other_min_scale,
        );
        if decision.notify {
            info!(
                max_scale = quake.earthquake.max_scale,
                time = %quake.earthquake.time,
                reason = %decision.reason,
                "テスト送信する報（震源未確定）を選択しました"
            );
            handle_text(config, http, &text, true, &mut DedupState::default()).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("震源未確定で通知条件を満たす報が履歴にありませんでした。しきい値（OTHER_MIN_SCALE 等）を下げて再試行してください");
    Ok(())
}

/// テスト用: 過去の緊急地震速報から通知条件を満たす最新の1件を選び、
/// 本番と同じ経路 (handle_eew) で Discord へ送信して終了する。
async fn run_test_eew(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の緊急地震速報を取得します");
    let body = http
        .get(HISTORY_EEW_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body)?;
    info!(count = items.len(), "履歴を取得しました");

    // テスト送信なので重複抑制は効かせない（毎回新しい状態を渡す）。
    let mut seen = SeenIds::default();
    for item in &items {
        let text = item.to_string();
        let eew: Eew = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if eew.cancelled {
            continue;
        }
        let decision = decide_eew(&eew.areas, &config.region_min_scales, config.other_min_scale);
        if decision.notify {
            info!(
                place = %eew.earthquake.hypocenter.name,
                event_id = %eew.issue.event_id,
                reason = %decision.reason,
                "テスト送信する緊急地震速報を選択しました"
            );
            handle_eew(config, http, &text, true, &mut seen).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に通知条件を満たす緊急地震速報がありませんでした。しきい値を下げて再試行してください");
    Ok(())
}

/// テスト用: 過去の津波予報から有効な1件を選び、本番経路 (handle_tsunami) で送信して終了する。
async fn run_test_tsunami(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の津波予報を取得します");
    let body = http
        .get(HISTORY_TSUNAMI_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body)?;
    info!(count = items.len(), "履歴を取得しました");

    let mut seen = SeenIds::default();
    for item in &items {
        let text = item.to_string();
        let tsunami: Tsunami = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let has_grade = tsunami
            .areas
            .iter()
            .any(|a| tsunami_grade_rank(&a.grade) > 0);
        if !tsunami.cancelled && has_grade {
            info!(areas = tsunami.areas.len(), "テスト送信する津波予報を選択しました");
            handle_tsunami(config, http, &text, true, &mut seen).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に津波予報がありませんでした（津波予報は稀に発表されます）");
    Ok(())
}

/// 受信した1メッセージ(JSON文字列)を code で振り分けて処理する。
///
/// `is_test` が true の場合、Discord 通知にテスト送信である旨を明示する。
async fn handle_text(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    dedup: &mut DedupState,
) -> Result<()> {
    // まず code だけ取り出して種別を判定する。
    let envelope: Envelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Ok(()), // 想定外のフォーマットは無視
    };

    match envelope.code {
        CODE_JMA_QUAKE => handle_quake(config, http, text, is_test, &mut dedup.quakes).await,
        CODE_EEW => handle_eew(config, http, text, is_test, &mut dedup.eews).await,
        CODE_TSUNAMI => handle_tsunami(config, http, text, is_test, &mut dedup.tsunamis).await,
        _ => Ok(()),
    }
}

/// 地震情報(551) を処理し、通知条件を満たせば通知する。
///
/// 同一地震（発生時刻キー）について速報（震度速報）と詳報（各地の震度など）を別管理し、
/// 内容が前回と同じなら投稿しない。内容が変わった場合は既存メッセージを差し替える（編集）。
async fn handle_quake(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    tracker: &mut QuakeTracker,
) -> Result<()> {
    let quake: JmaQuake = serde_json::from_str(text)?;
    let eq = &quake.earthquake;

    let decision = decide(
        eq,
        &quake.points,
        &config.region_min_scales,
        config.other_min_scale,
    );

    if !decision.notify {
        info!(
            max_scale = eq.max_scale,
            place = %eq.hypocenter.name,
            "通知条件を満たさないためスキップ"
        );
        return Ok(());
    }

    let is_prompt = quake.issue.is_prompt();
    let kind = if is_prompt { "震度速報" } else { "地震情報" };
    info!(
        kind,
        max_scale = eq.max_scale,
        place = %eq.hypocenter.name,
        reason = %decision.reason,
        "通知対象の地震を検出"
    );

    // 地図画像。震源座標が有効なら震源マップ、未確定（速報など）なら
    // 観測した都道府県ごとのマーカーマップにフォールバックする。
    // staticmap のタイル取得は同期通信なので spawn_blocking 上で実行する。
    let image = if config.attach_map {
        let tile_tpl = config.tile_url_template.clone();
        let result = if eq.hypocenter.has_valid_coords() {
            let lat = eq.hypocenter.latitude;
            let lon = eq.hypocenter.longitude;
            let scale = eq.max_scale;
            let markers = geo::points_to_markers(&quake.points);
            Some(
                tokio::task::spawn_blocking(move || {
                    mapgen::render_quake_map_with_points(lat, lon, scale, &markers, &tile_tpl)
                })
                .await?,
            )
        } else {
            let markers = geo::points_to_markers(&quake.points);
            if markers.is_empty() {
                None
            } else {
                Some(
                    tokio::task::spawn_blocking(move || {
                        mapgen::render_markers_map(&markers, &tile_tpl)
                    })
                    .await?,
                )
            }
        };

        match result {
            Some(Ok(bytes)) => Some(bytes),
            Some(Err(e)) => {
                warn!(error = %e, "地図画像の生成に失敗。テキストのみで通知します");
                None
            }
            None => None,
        }
    } else {
        None
    };

    let payload = discord::build_payload(&quake, &decision.reason, image.is_some(), is_test);
    let signature = signature_of(&payload);
    let key = eq.time.clone();

    // 発生時刻が不明な場合は重複判定・差し替えができないため、そのまま新規投稿する。
    if key.is_empty() {
        discord::send(http, &config.webhook_url, &payload, image).await?;
        info!(kind, "Discord へ通知しました（発生時刻不明のため重複判定なし）");
        return Ok(());
    }

    // 速報・詳報それぞれのスロットを取り出す。
    let entry = tracker.entry(&key);
    let slot = if is_prompt {
        &mut entry.prompt
    } else {
        &mut entry.detail
    };

    match slot {
        // 内容に変更なし → 投稿しない。
        Some(post) if post.signature == signature => {
            info!(kind, "内容に変更がないため投稿をスキップ");
        }
        // 内容が変わった → 既存メッセージを差し替え（編集）。
        Some(post) => {
            discord::edit_message(http, &config.webhook_url, &post.message_id, &payload, image)
                .await?;
            post.signature = signature;
            info!(kind, message_id = %post.message_id, "内容が変わったため差し替えました");
        }
        // 初報 → 新規投稿して message_id を記録する。
        None => {
            let message_id =
                discord::post_message(http, &config.webhook_url, &payload, image).await?;
            info!(kind, message_id = %message_id, "Discord へ通知しました");
            *slot = Some(QuakePost {
                signature,
                message_id,
            });
        }
    }

    Ok(())
}

/// 緊急地震速報(556) を処理する。
///
/// 第1報で速報し、同一 eventId の続報は抑制する。取消報は速報済みの場合のみ通知する。
async fn handle_eew(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    seen: &mut SeenIds,
) -> Result<()> {
    let eew: Eew = serde_json::from_str(text)?;
    let event_id = eew.issue.event_id.clone();

    // 取消報: 既に速報済みの地震だけ取消を通知する。
    if eew.cancelled {
        if event_id.is_empty() || (!is_test && !seen.contains(&event_id)) {
            return Ok(());
        }
        info!(event_id = %event_id, "緊急地震速報の取消を受信");
        let payload = discord::build_eew_payload(&eew, "", false, is_test);
        discord::send(http, &config.webhook_url, &payload, None).await?;
        info!("緊急地震速報の取消を通知しました");
        return Ok(());
    }

    let decision = decide_eew(&eew.areas, &config.region_min_scales, config.other_min_scale);
    if !decision.notify {
        return Ok(());
    }

    // 重複報の抑制: 同一 eventId は第1報のみ通知する。
    if event_id.is_empty() || !seen.mark_if_new(&event_id) {
        return Ok(());
    }

    info!(
        event_id = %event_id,
        serial = %eew.issue.serial,
        place = %eew.earthquake.hypocenter.name,
        reason = %decision.reason,
        "緊急地震速報を検出"
    );

    // 地図画像。震源座標が有効なら震源＋対象地域マーカーの地図、未確定なら
    // 対象地域マーカーのみの地図にフォールバックする（551 と同じ方針）。
    let image = if config.attach_map {
        let tile_tpl = config.tile_url_template.clone();
        let markers = geo::eew_areas_to_markers(&eew.areas);
        let result = if eew.earthquake.hypocenter.has_valid_coords() {
            let lat = eew.earthquake.hypocenter.latitude;
            let lon = eew.earthquake.hypocenter.longitude;
            let scale = eew_max_scale(&eew.areas);
            Some(
                tokio::task::spawn_blocking(move || {
                    mapgen::render_quake_map_with_points(lat, lon, scale, &markers, &tile_tpl)
                })
                .await?,
            )
        } else if markers.is_empty() {
            None
        } else {
            Some(
                tokio::task::spawn_blocking(move || mapgen::render_markers_map(&markers, &tile_tpl))
                    .await?,
            )
        };

        match result {
            Some(Ok(bytes)) => Some(bytes),
            Some(Err(e)) => {
                warn!(error = %e, "地図画像の生成に失敗。テキストのみで通知します");
                None
            }
            None => None,
        }
    } else {
        None
    };

    let payload = discord::build_eew_payload(&eew, &decision.reason, image.is_some(), is_test);
    discord::send(http, &config.webhook_url, &payload, image).await?;
    info!("緊急地震速報を通知しました");
    Ok(())
}

/// 津波予報(552) を処理する。
///
/// 同一発表(`id`)の再送は抑制する。発表・解除いずれも通知する。
async fn handle_tsunami(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    seen: &mut SeenIds,
) -> Result<()> {
    let tsunami: Tsunami = serde_json::from_str(text)?;

    // 同一発表の再送を除去する（id で重複判定）。
    if !tsunami.id.is_empty() && !seen.mark_if_new(&tsunami.id) {
        return Ok(());
    }

    if tsunami.cancelled {
        info!("津波予報の解除を受信");
        let payload = discord::build_tsunami_payload(&tsunami, is_test);
        discord::send(http, &config.webhook_url, &payload, None).await?;
        info!("津波予報の解除を通知しました");
        return Ok(());
    }

    // 有効な予報（注意報以上）が含まれない場合は通知しない。
    let max_rank = tsunami
        .areas
        .iter()
        .map(|a| tsunami_grade_rank(&a.grade))
        .max()
        .unwrap_or(0);
    if max_rank == 0 {
        return Ok(());
    }

    info!(areas = tsunami.areas.len(), "津波予報を検出");
    let payload = discord::build_tsunami_payload(&tsunami, is_test);
    discord::send(http, &config.webhook_url, &payload, None).await?;
    info!("津波予報を通知しました");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Earthquake, Hypocenter, JmaQuake, QuakeIssue};

    fn quake(issue_type: &str, max_scale: i32) -> JmaQuake {
        JmaQuake {
            code: 551,
            issue: QuakeIssue {
                issue_type: issue_type.to_string(),
            },
            earthquake: Earthquake {
                time: "2026/06/28 05:21:00".to_string(),
                max_scale,
                hypocenter: Hypocenter::default(),
                ..Default::default()
            },
            points: vec![],
        }
    }

    fn payload(issue_type: &str, max_scale: i32, reason: &str) -> serde_json::Value {
        discord::build_payload(&quake(issue_type, max_scale), reason, false, false)
    }

    #[test]
    fn signature_same_for_identical_content() {
        let a = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        let b = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        assert_eq!(signature_of(&a), signature_of(&b));
    }

    #[test]
    fn signature_differs_when_scale_changes() {
        let a = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        let b = payload("ScalePrompt", 50, "東北で最大震度5強を観測");
        assert_ne!(signature_of(&a), signature_of(&b));
    }

    #[test]
    fn signature_differs_between_prompt_and_detail() {
        // 速報と詳報はタイトルが変わるためシグネチャも異なる（別管理の裏付け）。
        let prompt = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        let detail = payload("DetailScale", 45, "東北で最大震度5弱を観測");
        assert_ne!(signature_of(&prompt), signature_of(&detail));
    }

    #[test]
    fn tracker_keeps_prompt_and_detail_separately() {
        let mut tracker = QuakeTracker::default();
        let key = "2026/06/28 05:21:00";
        {
            let entry = tracker.entry(key);
            entry.prompt = Some(QuakePost {
                signature: 1,
                message_id: "p".to_string(),
            });
        }
        let entry = tracker.entry(key);
        assert!(entry.prompt.is_some());
        assert!(entry.detail.is_none());
    }

    #[test]
    fn tracker_evicts_oldest_over_capacity() {
        let mut tracker = QuakeTracker::default();
        for i in 0..(SEEN_ID_CAPACITY + 5) {
            tracker.entry(&format!("key-{i}"));
        }
        assert_eq!(tracker.map.len(), SEEN_ID_CAPACITY);
        // 最初に入れたキーは退避されている。
        assert!(!tracker.map.contains_key("key-0"));
    }
}
