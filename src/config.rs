//! 環境変数による設定。

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Discord Webhook URL。
    pub webhook_url: String,
    /// P2P地震情報 WebSocket エンドポイント。
    pub ws_url: String,
    /// 関東で通知する最小スケール（既定 40 = 震度4）。
    pub kanto_min_scale: i32,
    /// 関東以外で通知する最小スケール（既定 50 = 震度5強）。
    pub other_min_scale: i32,
    /// 地図画像を添付するか。
    pub attach_map: bool,
    /// 地図タイルの URL テンプレート。
    pub tile_url_template: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let webhook_url = std::env::var("DISCORD_WEBHOOK_URL")
            .context("環境変数 DISCORD_WEBHOOK_URL が未設定です")?;

        let ws_url = std::env::var("P2PQUAKE_WS_URL")
            .unwrap_or_else(|_| "wss://api.p2pquake.net/v2/ws".to_string());

        let kanto_min_scale = parse_scale("KANTO_MIN_SCALE", 40)?;
        let other_min_scale = parse_scale("OTHER_MIN_SCALE", 50)?;

        let attach_map = std::env::var("ATTACH_MAP")
            .map(|v| !matches!(v.to_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);

        let tile_url_template = std::env::var("TILE_URL_TEMPLATE")
            .unwrap_or_else(|_| "https://tile.openstreetmap.org/{z}/{x}/{y}.png".to_string());

        Ok(Config {
            webhook_url,
            ws_url,
            kanto_min_scale,
            other_min_scale,
            attach_map,
            tile_url_template,
        })
    }
}

fn parse_scale(key: &str, default: i32) -> Result<i32> {
    match std::env::var(key) {
        Ok(v) => v
            .parse::<i32>()
            .with_context(|| format!("環境変数 {key} は整数で指定してください: {v}")),
        Err(_) => Ok(default),
    }
}
