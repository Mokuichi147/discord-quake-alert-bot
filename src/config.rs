//! TOMLで記述した通知設定と、Webhook URLの環境変数を読み込む。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;

use crate::intensity::REGIONS;

const DEFAULT_WS_URL: &str = "wss://api.p2pquake.net/v2/ws";
const DEFAULT_TILE_URL_TEMPLATE: &str =
    "https://cyberjapandata.gsi.go.jp/xyz/blank/{z}/{x}/{y}.png";

#[derive(Debug, Clone)]
pub struct Config {
    /// 全通知設定で共有する WebSocket エンドポイント。
    pub ws_url: String,
    pub webhooks: Vec<WebhookConfig>,
}

/// 1つのWebhookへ送る通知設定。
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// TOMLの`[webhooks.<name>]`にある`<name>`。
    pub name: String,
    pub webhook_url: String,
    /// この送信先で強調表示する地域・観測点。
    pub watched_points: Vec<WatchedPoint>,
    /// 地方ごとの通知する最小スケール。未設定の地方は`other_min_scale`。
    pub region_min_scales: HashMap<String, i32>,
    pub other_min_scale: i32,
    pub attach_map: bool,
    pub tile_url_template: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlConfig {
    /// 省略時は`P2PQUAKE_WS_URL`、それも無ければ公式の既定値を使う。
    #[serde(default)]
    ws_url: Option<String>,
    #[serde(default)]
    webhooks: BTreeMap<String, TomlWebhookConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlWebhookConfig {
    /// URLそのものを設定ファイルに書かず、この名前の環境変数から読む。
    webhook_url_env: String,
    #[serde(default)]
    watched_points: Vec<WatchedPoint>,
    #[serde(default)]
    region_min_scales: HashMap<String, i32>,
    #[serde(default = "default_other_min_scale")]
    other_min_scale: i32,
    #[serde(default = "default_attach_map")]
    attach_map: bool,
    #[serde(default = "default_tile_url_template")]
    tile_url_template: String,
}

fn default_other_min_scale() -> i32 {
    50
}

fn default_attach_map() -> bool {
    true
}

fn default_tile_url_template() -> String {
    DEFAULT_TILE_URL_TEMPLATE.to_string()
}

impl Config {
    /// 指定したTOML設定ファイルを読み込む。
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).with_context(|| {
            format!(
                "設定ファイル {} を読み込めません。config.example.tomlをコピーして作成してください",
                path.display()
            )
        })?;
        Self::from_toml_str(&contents, |key| std::env::var(key).ok())
            .with_context(|| format!("設定ファイル {} の設定が不正です", path.display()))
    }

    /// プレビュー用に、Webhook URLを検証せず`main`、または名前順で最初のWebhookの
    /// タイル設定だけを読む。
    ///
    /// 設定ファイルがまだ無い場合は、通知設定を用意していない状態でもプレビューを
    /// 実行できるよう既定のタイルURLを返す。
    pub fn tile_url_template_from_file(path: impl AsRef<Path>) -> Result<String> {
        let path = path.as_ref();
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DEFAULT_TILE_URL_TEMPLATE.to_string())
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("設定ファイル {} を読み込めません", path.display()))
            }
        };
        let raw: TomlConfig = toml::from_str(&contents).context("TOMLの構文が不正です")?;
        raw.webhooks
            .get("main")
            .or_else(|| raw.webhooks.values().next())
            .map(|webhook| webhook.tile_url_template.clone())
            .context("webhooksを1つ以上設定してください")
    }

    fn from_toml_str(contents: &str, get_env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let raw: TomlConfig = toml::from_str(contents).context("TOMLの構文が不正です")?;
        ensure!(
            !raw.webhooks.is_empty(),
            "webhooksを1つ以上設定してください"
        );

        let ws_url = raw
            .ws_url
            .or_else(|| get_env("P2PQUAKE_WS_URL"))
            .unwrap_or_else(|| DEFAULT_WS_URL.to_string());
        ensure!(!ws_url.trim().is_empty(), "ws_urlは空にできません");

        let mut webhooks = Vec::with_capacity(raw.webhooks.len());
        for (name, webhook) in raw.webhooks {
            ensure!(!name.trim().is_empty(), "webhooksの名前は空にできません");
            webhooks.push(WebhookConfig::from_toml(&name, webhook, &get_env)?);
        }

        Ok(Self { ws_url, webhooks })
    }
}

impl WebhookConfig {
    fn from_toml(
        name: &str,
        raw: TomlWebhookConfig,
        get_env: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let env_name = raw.webhook_url_env.trim();
        ensure!(
            !env_name.is_empty(),
            "webhooks.{name}.webhook_url_envは必須です"
        );
        let webhook_url = get_env(env_name)
            .filter(|value| !value.trim().is_empty())
            .with_context(|| {
                format!("webhooks.{name}が参照する環境変数{env_name}が未設定または空です")
            })?;

        for region in raw.region_min_scales.keys() {
            ensure!(
                REGIONS.iter().any(|(known, _)| *known == region),
                "webhooks.{name}.region_min_scalesに不明な地方があります: {region}"
            );
        }

        Ok(Self {
            name: name.to_string(),
            webhook_url,
            watched_points: normalize_watched_points(raw.watched_points)
                .with_context(|| format!("webhooks.{name}.watched_pointsが不正です"))?,
            region_min_scales: raw.region_min_scales,
            other_min_scale: raw.other_min_scale,
            attach_map: raw.attach_map,
            tile_url_template: raw.tile_url_template,
        })
    }
}

/// 情報源の都道府県と地域・観測点名で照合する登録地点。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WatchedPoint {
    pub pref: String,
    pub name: String,
    #[serde(default)]
    pub label: String,
}

fn normalize_watched_points(mut points: Vec<WatchedPoint>) -> Result<Vec<WatchedPoint>> {
    for (i, point) in points.iter_mut().enumerate() {
        point.pref = point.pref.trim().to_string();
        point.name = point.name.trim().to_string();
        point.label = point.label.trim().to_string();
        ensure!(
            !point.pref.is_empty() && !point.name.is_empty(),
            "{}番目: prefとnameは必須です",
            i + 1
        );
        for value in [&point.pref, &point.name, &point.label] {
            ensure!(
                value.chars().count() <= 100 && !value.chars().any(char::is_control),
                "{}番目: 各項目は改行なしの100文字以内で指定してください",
                i + 1
            );
        }
    }

    // 登録順を維持し、まったく同じ設定の重複を除く。
    let mut unique = Vec::with_capacity(points.len());
    for point in points {
        if !unique.contains(&point) {
            unique.push(point);
        }
    }
    Ok(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(contents: &str, env: &[(&str, &str)]) -> Result<Config> {
        Config::from_toml_str(contents, |key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn toml_loads_multiple_webhooks_and_keeps_names() {
        let config = config(
            r#"
ws_url = "ws://localhost/ws"

[webhooks.home]
webhook_url_env = "DISCORD_WEBHOOK_HOME"
watched_points = [
  { pref = " 東京都 ", name = "東京都23区", label = " 自宅周辺 " },
  { pref = "東京都", name = "東京都23区", label = "自宅周辺" },
]
other_min_scale = 40
region_min_scales = { "関東" = 30, "近畿" = 45 }
tile_url_template = "home-tiles"

[webhooks.office]
webhook_url_env = "DISCORD_WEBHOOK_OFFICE"
other_min_scale = 55
region_min_scales = { "東北" = 50, "関東" = 45 }
attach_map = false
"#,
            &[
                ("DISCORD_WEBHOOK_HOME", "https://example.com/home"),
                ("DISCORD_WEBHOOK_OFFICE", "https://example.com/office"),
            ],
        )
        .unwrap();

        assert_eq!(config.ws_url, "ws://localhost/ws");
        assert_eq!(config.webhooks.len(), 2);
        let home = &config.webhooks[0];
        let office = &config.webhooks[1];
        assert_eq!(home.name, "home");
        assert_eq!(home.webhook_url, "https://example.com/home");
        assert_eq!(home.watched_points.len(), 1);
        assert_eq!(home.watched_points[0].label, "自宅周辺");
        assert_eq!(home.region_min_scales["関東"], 30);
        assert_eq!(home.other_min_scale, 40);
        assert_eq!(home.tile_url_template, "home-tiles");
        assert_eq!(office.name, "office");
        assert_eq!(office.webhook_url, "https://example.com/office");
        assert!(!office.attach_map);
        assert_eq!(office.region_min_scales["東北"], 50);
    }

    #[test]
    fn defaults_are_applied() {
        let config = config(
            r#"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
"#,
            &[
                ("P2PQUAKE_WS_URL", "ws://env/ws"),
                ("DISCORD_WEBHOOK_URL", "url"),
            ],
        )
        .unwrap();
        assert_eq!(config.ws_url, "ws://env/ws");
        let hook = &config.webhooks[0];
        assert_eq!(hook.other_min_scale, 50);
        assert!(hook.attach_map);
        assert_eq!(hook.tile_url_template, DEFAULT_TILE_URL_TEMPLATE);
        assert!(hook.watched_points.is_empty());
    }

    #[test]
    fn explicit_ws_url_takes_precedence_over_environment() {
        let config = config(
            r#"
ws_url = "ws://toml/ws"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
"#,
            &[
                ("P2PQUAKE_WS_URL", "ws://env/ws"),
                ("DISCORD_WEBHOOK_URL", "url"),
            ],
        )
        .unwrap();
        assert_eq!(config.ws_url, "ws://toml/ws");
    }

    #[test]
    fn invalid_settings_are_rejected_without_exposing_secret() {
        let secret = "https://discord.com/api/webhooks/private-secret";
        let error = config(
            r#"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
region_min_scales = { "関東圏" = 40 }
"#,
            &[("DISCORD_WEBHOOK_URL", secret)],
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("不明な地方"));
        assert!(!format!("{error:#}").contains(secret));

        assert!(config(
            "[webhooks.main]\nwebhook_url_env = \"DISCORD_WEBHOOK_URL\"\n",
            &[],
        )
        .is_err());
        assert!(config("", &[]).is_err());
        assert!(config("[webhooks]\n", &[]).is_err());
    }

    #[test]
    fn watched_points_are_deduplicated_and_validated() {
        let loaded = config(
            r#"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
watched_points = [
  { pref = "東京都", name = "東京都23区", label = "自宅" },
  { pref = "東京都", name = "東京都23区", label = "自宅" },
]
"#,
            &[("DISCORD_WEBHOOK_URL", "url")],
        )
        .unwrap();
        assert_eq!(loaded.webhooks[0].watched_points.len(), 1);
        assert!(config(
            r#"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
watched_points = [{ pref = " ", name = "東京都23区" }]
"#,
            &[("DISCORD_WEBHOOK_URL", "url")],
        )
        .is_err());
    }

    #[test]
    fn unknown_toml_fields_are_rejected() {
        let error = config(
            r#"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
watched_point = []
"#,
            &[("DISCORD_WEBHOOK_URL", "url")],
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("TOMLの構文が不正です"));
    }

    #[test]
    fn example_config_is_valid() {
        let contents = include_str!("../config.example.toml");
        let config = Config::from_toml_str(contents, |key| {
            (key == "DISCORD_WEBHOOK_URL").then(|| "https://example.com/hook".to_string())
        })
        .unwrap();
        assert_eq!(config.webhooks.len(), 1);
        assert_eq!(config.webhooks[0].name, "main");
    }

    #[test]
    fn preview_tile_setting_does_not_require_webhook_secret() {
        let path = std::env::temp_dir().join(format!(
            "quake-alert-preview-config-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[webhooks.main]
webhook_url_env = "DISCORD_WEBHOOK_URL"
tile_url_template = "preview-tiles"
"#,
        )
        .unwrap();
        let tile_url = Config::tile_url_template_from_file(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(tile_url, "preview-tiles");
    }
}
