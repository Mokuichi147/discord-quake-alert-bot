//! 共通の接続設定と、独立した通知設定ファイルの読み込み。

use std::collections::{HashMap, HashSet};

use anyhow::{ensure, Context, Result};

use crate::intensity::REGIONS;

#[derive(Debug, Clone)]
pub struct Config {
    /// 全通知設定で共有する WebSocket エンドポイント。
    pub ws_url: String,
    pub webhooks: Vec<WebhookConfig>,
}

/// 従来の通知設定一式。設定ごとに送信先と全地方の条件を指定できる。
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// ログに表示する設定ファイル名。
    pub name: String,
    pub webhook_url: String,
    /// 地方ごとの通知する最小スケール。未設定の地方は `other_min_scale`。
    pub region_min_scales: HashMap<String, i32>,
    pub other_min_scale: i32,
    pub attach_map: bool,
    pub tile_url_template: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_sources(|key| std::env::var(key).ok(), read_notification_file)
    }

    /// 通知ファイルは環境変数へ書き込まず、それぞれ独立して解釈する。
    fn from_sources(
        get: impl Fn(&str) -> Option<String>,
        read: impl Fn(&str) -> Result<HashMap<String, String>>,
    ) -> Result<Self> {
        let ws_url =
            get("P2PQUAKE_WS_URL").unwrap_or_else(|| "wss://api.p2pquake.net/v2/ws".to_string());
        let webhooks = if let Some(paths) = get("NOTIFICATION_CONFIG_FILES") {
            let mut seen = HashSet::new();
            let mut webhooks = Vec::new();
            for path in paths.split(',').map(str::trim) {
                ensure!(
                    !path.is_empty(),
                    "NOTIFICATION_CONFIG_FILES に空のファイル名があります"
                );
                ensure!(
                    seen.insert(path),
                    "通知設定ファイルが重複しています: {path}"
                );
                let values = read(path)
                    .with_context(|| format!("通知設定ファイル {path} を読み込めません"))?;
                webhooks.push(
                    WebhookConfig::from_lookup(path, |key| values.get(key).cloned())
                        .with_context(|| format!("通知設定ファイル {path} の設定が不正です"))?,
                );
            }
            webhooks
        } else {
            // ファイル一覧を指定しない場合は、従来どおり .env / 環境変数を使う。
            vec![WebhookConfig::from_lookup("default", get)?]
        };
        Ok(Self { ws_url, webhooks })
    }
}

fn read_notification_file(path: &str) -> Result<HashMap<String, String>> {
    // dotenv() と違い、プロセスの環境変数を上書きしない。
    let entries = dotenvy::from_path_iter(path)?;
    let mut values = HashMap::new();
    for entry in entries {
        let (key, value) = entry?;
        values.insert(key, value);
    }
    Ok(values)
}

impl WebhookConfig {
    fn from_lookup(name: &str, get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let webhook_url = get("DISCORD_WEBHOOK_URL")
            .filter(|v| !v.trim().is_empty())
            .context("DISCORD_WEBHOOK_URL が未設定または空です")?;
        let scale = |key: &str| -> Result<Option<i32>> {
            get(key)
                .map(|v| {
                    v.parse::<i32>()
                        .with_context(|| format!("{key} は整数で指定してください: {v}"))
                })
                .transpose()
        };
        let mut region_min_scales = HashMap::new();
        for (region, prefix, _) in REGIONS {
            if let Some(value) = scale(&format!("{prefix}_MIN_SCALE"))? {
                region_min_scales.insert(region.to_string(), value);
            }
        }
        let other_min_scale = scale("OTHER_MIN_SCALE")?.unwrap_or(50);
        let attach_map = get("ATTACH_MAP")
            .map(|v| !matches!(v.to_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        let tile_url_template = get("TILE_URL_TEMPLATE").unwrap_or_else(|| {
            "https://cyberjapandata.gsi.go.jp/xyz/blank/{z}/{x}/{y}.png".to_string()
        });
        Ok(Self {
            name: name.to_string(),
            webhook_url,
            region_min_scales,
            other_min_scale,
            attach_map,
            tile_url_template,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(values: &[(&str, &str)], key: &str) -> Option<String> {
        values
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string())
    }

    fn config(env: &[(&str, &str)], files: &[(&str, &[(&str, &str)])]) -> Result<Config> {
        Config::from_sources(
            |key| lookup(env, key),
            |path| {
                let (_, values) = files
                    .iter()
                    .find(|(p, _)| *p == path)
                    .with_context(|| format!("ファイルがありません: {path}"))?;
                Ok(values
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect())
            },
        )
    }

    #[test]
    fn legacy_settings_remain_compatible() {
        let config = config(
            &[
                ("DISCORD_WEBHOOK_URL", "https://example.com/legacy"),
                ("KANTO_MIN_SCALE", "40"),
                ("ATTACH_MAP", "no"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(config.webhooks.len(), 1);
        let hook = &config.webhooks[0];
        assert_eq!(hook.name, "default");
        assert_eq!(hook.webhook_url, "https://example.com/legacy");
        assert_eq!(hook.region_min_scales["関東"], 40);
        assert_eq!(hook.other_min_scale, 50);
        assert!(!hook.attach_map);
    }

    #[test]
    fn complete_notification_settings_are_independent() {
        let config = config(
            &[
                ("NOTIFICATION_CONFIG_FILES", " first.env, second.env "),
                ("DISCORD_WEBHOOK_URL", "unused"),
                ("OTHER_MIN_SCALE", "70"),
                ("KANTO_MIN_SCALE", "70"),
                ("ATTACH_MAP", "false"),
                ("P2PQUAKE_WS_URL", "ws://localhost/ws"),
            ],
            &[
                (
                    "first.env",
                    &[
                        ("DISCORD_WEBHOOK_URL", "first"),
                        ("OTHER_MIN_SCALE", "40"),
                        ("TOHOKU_MIN_SCALE", "30"),
                        ("KANTO_MIN_SCALE", "30"),
                        ("KINKI_MIN_SCALE", "45"),
                        ("TILE_URL_TEMPLATE", "first-tiles"),
                    ],
                ),
                (
                    "second.env",
                    &[
                        ("DISCORD_WEBHOOK_URL", "second"),
                        ("OTHER_MIN_SCALE", "55"),
                        ("TOHOKU_MIN_SCALE", "50"),
                        ("KANTO_MIN_SCALE", "45"),
                        ("ATTACH_MAP", "false"),
                    ],
                ),
            ],
        )
        .unwrap();
        assert_eq!(config.ws_url, "ws://localhost/ws");
        assert_eq!(config.webhooks.len(), 2);
        let first = &config.webhooks[0];
        let second = &config.webhooks[1];
        assert_eq!(first.webhook_url, "first");
        assert_eq!(second.webhook_url, "second");
        assert_eq!(first.other_min_scale, 40);
        assert_eq!(second.other_min_scale, 55);
        assert_eq!(first.region_min_scales["東北"], 30);
        assert_eq!(first.region_min_scales["関東"], 30);
        assert_eq!(first.region_min_scales["近畿"], 45);
        assert_eq!(second.region_min_scales["東北"], 50);
        assert_eq!(second.region_min_scales["関東"], 45);
        assert!(!second.region_min_scales.contains_key("近畿"));
        assert!(first.attach_map);
        assert!(!second.attach_map);
        assert_eq!(first.tile_url_template, "first-tiles");
        assert_ne!(second.tile_url_template, "first-tiles");
    }

    #[test]
    fn nationwide_settings_do_not_require_regions_or_inherit_environment() {
        let config = config(
            &[
                ("NOTIFICATION_CONFIG_FILES", "one,two"),
                ("KANTO_MIN_SCALE", "10"),
                ("OTHER_MIN_SCALE", "70"),
            ],
            &[
                (
                    "one",
                    &[("DISCORD_WEBHOOK_URL", "one"), ("OTHER_MIN_SCALE", "40")],
                ),
                ("two", &[("DISCORD_WEBHOOK_URL", "two")]),
            ],
        )
        .unwrap();
        assert!(config
            .webhooks
            .iter()
            .all(|hook| hook.region_min_scales.is_empty()));
        assert_eq!(config.webhooks[0].other_min_scale, 40);
        assert_eq!(config.webhooks[1].other_min_scale, 50);
    }

    #[test]
    fn empty_duplicate_and_missing_files_are_rejected() {
        for paths in ["", "one,", "one,one", "missing"] {
            assert!(config(
                &[("NOTIFICATION_CONFIG_FILES", paths)],
                &[("one", &[("DISCORD_WEBHOOK_URL", "url")])]
            )
            .is_err());
        }
    }

    #[test]
    fn each_file_requires_a_webhook_and_reports_invalid_settings() {
        let error = config(
            &[
                ("NOTIFICATION_CONFIG_FILES", "one"),
                ("DISCORD_WEBHOOK_URL", "secret"),
            ],
            &[("one", &[])],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("one"));
        assert!(message.contains("DISCORD_WEBHOOK_URL"));
        assert!(!message.contains("secret"));
        assert!(config(&[("DISCORD_WEBHOOK_URL", " ")], &[]).is_err());
        let error = config(
            &[("NOTIFICATION_CONFIG_FILES", "one")],
            &[(
                "one",
                &[("DISCORD_WEBHOOK_URL", "url"), ("OTHER_MIN_SCALE", "bad")],
            )],
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("OTHER_MIN_SCALE"));
    }

    #[test]
    fn dotenv_files_keep_existing_keys_and_syntax() {
        let path =
            std::env::temp_dir().join(format!("quake-alert-config-{}.env", std::process::id()));
        std::fs::write(&path, "# 通知設定\nDISCORD_WEBHOOK_URL='https://example.com/hook'\nOTHER_MIN_SCALE=40\nKANTO_MIN_SCALE=30\nATTACH_MAP=false\n").unwrap();
        let values = read_notification_file(path.to_str().unwrap());
        std::fs::remove_file(path).unwrap();
        let values = values.unwrap();
        let hook = WebhookConfig::from_lookup("file", |key| values.get(key).cloned()).unwrap();
        assert_eq!(hook.webhook_url, "https://example.com/hook");
        assert_eq!(hook.other_min_scale, 40);
        assert_eq!(hook.region_min_scales["関東"], 30);
        assert!(!hook.attach_map);
    }
}
