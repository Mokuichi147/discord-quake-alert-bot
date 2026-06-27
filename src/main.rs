//! 地震bot エントリポイント。
//!
//! P2P地震情報の WebSocket を購読し、関東を中心とした強い揺れの地震を
//! 地図画像付きで Discord Webhook へ通知する。

mod config;
mod discord;
mod intensity;
mod mapgen;
mod model;

use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::intensity::decide;
use crate::model::{Envelope, JmaQuake};

/// 地震情報メッセージの code。
const CODE_JMA_QUAKE: i32 = 551;

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
        kanto_min = config.kanto_min_scale,
        other_min = config.other_min_scale,
        attach_map = config.attach_map,
        "地震botを起動しました"
    );

    let http = reqwest::Client::builder()
        .user_agent("quake-alert-bot/0.1 (+https://github.com/)")
        .timeout(Duration::from_secs(30))
        .build()?;

    // `--test` 指定時は、過去の地震情報を1件取得して Discord へ送信し終了する。
    if std::env::args().any(|a| a == "--test") {
        return run_test(&config, &http).await;
    }

    // 切断されても再接続し続ける。失敗時は指数バックオフ（最大60秒）。
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_once(&config, &http).await {
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
async fn run_once(config: &Config, http: &reqwest::Client) -> Result<()> {
    let (ws_stream, _resp) = tokio_tungstenite::connect_async(config.ws_url.as_str()).await?;
    info!("WebSocket に接続しました");
    let (_write, mut read) = ws_stream.split();

    while let Some(message) = read.next().await {
        let message = message?;
        match message {
            Message::Text(text) => {
                if let Err(e) = handle_text(config, http, &text, false).await {
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
            config.kanto_min_scale,
            config.other_min_scale,
        );
        if decision.notify {
            info!(
                place = %quake.earthquake.hypocenter.name,
                max_scale = quake.earthquake.max_scale,
                time = %quake.earthquake.time,
                "テスト送信する地震を選択しました"
            );
            handle_text(config, http, &text, true).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に通知条件を満たす地震がありませんでした。しきい値を下げて再試行してください");
    Ok(())
}

/// 受信した1メッセージ(JSON文字列)を処理する。
///
/// `is_test` が true の場合、Discord 通知にテスト送信である旨を明示する。
async fn handle_text(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
) -> Result<()> {
    // まず code だけ取り出して種別を判定する。
    let envelope: Envelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Ok(()), // 想定外のフォーマットは無視
    };

    if envelope.code != CODE_JMA_QUAKE {
        return Ok(());
    }

    let quake: JmaQuake = serde_json::from_str(text)?;
    let eq = &quake.earthquake;

    let decision = decide(
        eq,
        &quake.points,
        config.kanto_min_scale,
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
