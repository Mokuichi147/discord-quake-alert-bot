//! 地震bot エントリポイント。
//!
//! P2P地震情報の WebSocket を購読し、関東を中心とした強い揺れの地震を
//! 地図画像付きで Discord Webhook へ通知する。

mod config;
mod discord;
mod intensity;
mod mapgen;
mod model;

use std::collections::{HashSet, VecDeque};
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

/// 種別ごとの重複抑制状態。
#[derive(Default)]
struct DedupState {
    /// 緊急地震速報の eventId（第1報のみ通知）。
    eews: SeenIds,
    /// 津波予報の id（同一発表の再送を除去）。
    tsunamis: SeenIds,
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
        CODE_JMA_QUAKE => handle_quake(config, http, text, is_test).await,
        CODE_EEW => handle_eew(config, http, text, is_test, &mut dedup.eews).await,
        CODE_TSUNAMI => handle_tsunami(config, http, text, is_test, &mut dedup.tsunamis).await,
        _ => Ok(()),
    }
}

/// 地震情報(551) を処理し、通知条件を満たせば確定情報を通知する。
async fn handle_quake(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
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

    info!(
        max_scale = eq.max_scale,
        place = %eq.hypocenter.name,
        reason = %decision.reason,
        "通知対象の地震を検出"
    );

    // 地図画像（座標が有効かつ設定が有効な場合のみ）。
    // staticmap のタイル取得は同期通信なので spawn_blocking 上で実行する。
    let image = if config.attach_map && eq.hypocenter.has_valid_coords() {
        let lat = eq.hypocenter.latitude;
        let lon = eq.hypocenter.longitude;
        let scale = eq.max_scale;
        let tile_tpl = config.tile_url_template.clone();

        let result = tokio::task::spawn_blocking(move || {
            mapgen::render_quake_map(lat, lon, scale, &tile_tpl)
        })
        .await?;

        match result {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                warn!(error = %e, "地図画像の生成に失敗。テキストのみで通知します");
                None
            }
        }
    } else {
        None
    };

    let payload = discord::build_payload(&quake, &decision.reason, image.is_some(), is_test);
    discord::send(http, &config.webhook_url, &payload, image).await?;
    info!("Discord へ通知しました");
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

    // 地図画像（座標が有効かつ設定が有効な場合のみ）。
    let image = if config.attach_map && eew.earthquake.hypocenter.has_valid_coords() {
        let lat = eew.earthquake.hypocenter.latitude;
        let lon = eew.earthquake.hypocenter.longitude;
        let scale = eew_max_scale(&eew.areas);
        let tile_tpl = config.tile_url_template.clone();

        let result = tokio::task::spawn_blocking(move || {
            mapgen::render_quake_map(lat, lon, scale, &tile_tpl)
        })
        .await?;

        match result {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                warn!(error = %e, "地図画像の生成に失敗。テキストのみで通知します");
                None
            }
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
