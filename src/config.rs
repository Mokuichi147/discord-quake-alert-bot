//! 環境変数による設定。

use std::collections::HashMap;

use anyhow::{Context, Result};

use crate::intensity::REGIONS;

#[derive(Debug, Clone)]
pub struct Config {
    /// Discord Webhook URL。
    pub webhook_url: String,
    /// P2P地震情報 WebSocket エンドポイント。
    pub ws_url: String,
    /// 地方ごとの通知する最小スケール（地方名→スケール）。未設定の地方は `other_min_scale`。
    pub region_min_scales: HashMap<String, i32>,
    /// どの地方区分にも該当しない（または未設定の）場合の最小スケール（既定 50 = 震度5強）。
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

        // 地方ごとの下限。環境変数が設定された地方のみ登録し、未設定の地方は
        // other_min_scale にフォールバックする（特定地方の特別扱いはしない）。
        let mut region_min_scales = HashMap::new();
        for (name, prefix, _) in REGIONS {
            let key = format!("{prefix}_MIN_SCALE");
            if let Some(v) = optional_scale(&key)? {
                region_min_scales.insert(name.to_string(), v);
            }
        }
        let other_min_scale = parse_scale("OTHER_MIN_SCALE", 50)?;

        let attach_map = std::env::var("ATTACH_MAP")
            .map(|v| !matches!(v.to_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);

        let tile_url_template = std::env::var("TILE_URL_TEMPLATE").unwrap_or_else(|_| {
            "https://cyberjapandata.gsi.go.jp/xyz/blank/{z}/{x}/{y}.png".to_string()
        });

        Ok(Config {
            webhook_url,
            ws_url,
            region_min_scales,
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

/// 環境変数があれば整数として解釈し `Some`、未設定なら `None` を返す。
fn optional_scale(key: &str) -> Result<Option<i32>> {
    match std::env::var(key) {
        Ok(v) => v
            .parse::<i32>()
            .map(Some)
            .with_context(|| format!("環境変数 {key} は整数で指定してください: {v}")),
        Err(_) => Ok(None),
    }
}
