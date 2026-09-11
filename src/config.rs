//! 共通の接続設定と、独立した通知設定ファイルの読み込み。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{anyhow, ensure};
use anyhow::{Context, Result};
use serde::Deserialize;

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
    /// この送信先で強調表示する地域・観測点。
    pub watched_points: Vec<WatchedPoint>,
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
        Self::from_sources_with_resolver(get, read, canonicalize_notification_path)
    }

    fn from_sources_with_resolver(
        get: impl Fn(&str) -> Option<String>,
        read: impl Fn(&str) -> Result<HashMap<String, String>>,
        resolve: impl Fn(&str) -> Result<PathBuf>,
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
                let canonical_path = resolve(path)
                    .with_context(|| format!("通知設定ファイル {path} のパスを解決できません"))?;
                ensure!(
                    seen.insert(canonical_path),
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

fn canonicalize_notification_path(path: &str) -> Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("通知設定ファイル {path} が存在しません"))
}

fn read_notification_file(path: &str) -> Result<HashMap<String, String>> {
    // dotenvy の変数展開はプロセス環境を参照するため、先に `$` をエスケープする。
    // 通知設定ファイルはファイル内の値だけで完結させ、起動用環境変数を継承しない。
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("通知設定ファイル {path} を読み込めません"))?;
    let contents = escape_dollar_expansion(&contents);
    let entries = dotenvy::from_read_iter(std::io::Cursor::new(contents));
    let mut values = HashMap::new();
    for entry in entries {
        let (key, value) = entry.map_err(|error| sanitize_dotenv_error(path, error))?;
        values.insert(key, value);
    }
    Ok(values)
}

/// dotenvy の構文を維持したまま、変数展開の記号だけをリテラルにする。
fn escape_dollar_expansion(contents: &str) -> String {
    let mut escaped = String::with_capacity(contents.len());
    let mut single_quote = false;
    let mut double_quote = false;
    let mut backslash = false;
    let mut expecting_end = false;
    let mut line_start = true;
    let mut only_whitespace = true;
    let mut comment = false;

    for character in contents.chars() {
        if comment {
            escaped.push(character);
            if character == '\n' {
                comment = false;
                line_start = true;
                only_whitespace = true;
                expecting_end = false;
            }
            continue;
        }

        if line_start && !single_quote && !double_quote {
            if only_whitespace && matches!(character, ' ' | '\t' | '\r') {
                escaped.push(character);
                continue;
            }
            if only_whitespace && character == '#' {
                escaped.push(character);
                comment = true;
                line_start = false;
                continue;
            }
            line_start = false;
            only_whitespace = false;
        }

        if single_quote {
            escaped.push(character);
            if character == '\'' {
                single_quote = false;
            }
            continue;
        }
        if backslash {
            escaped.push(character);
            backslash = false;
            continue;
        }
        if double_quote {
            if character == '\\' {
                escaped.push(character);
                backslash = true;
            } else if character == '"' {
                escaped.push(character);
                double_quote = false;
            } else if character == '$' {
                escaped.push('\\');
                escaped.push('$');
            } else {
                escaped.push(character);
            }
            continue;
        }

        if expecting_end {
            if matches!(character, ' ' | '\t') {
                escaped.push(character);
                continue;
            }
            if character == '#' {
                escaped.push(character);
                comment = true;
                expecting_end = false;
                continue;
            }
            expecting_end = false;
        }

        match character {
            '\\' => {
                escaped.push(character);
                backslash = true;
            }
            '\'' => {
                escaped.push(character);
                single_quote = true;
            }
            '"' => {
                escaped.push(character);
                double_quote = true;
            }
            '$' => {
                escaped.push('\\');
                escaped.push('$');
            }
            ' ' | '\t' => {
                escaped.push(character);
                expecting_end = true;
            }
            '\n' => {
                escaped.push(character);
                line_start = true;
                only_whitespace = true;
                expecting_end = false;
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

fn sanitize_dotenv_error(path: &str, error: dotenvy::Error) -> anyhow::Error {
    match error {
        // dotenvy の LineParse は秘密値を含む行全体を保持しているため、表示しない。
        dotenvy::Error::LineParse(_, position) => {
            anyhow!("通知設定ファイル {path} の構文エラー（行内位置: {position}）")
        }
        dotenvy::Error::Io(error) => {
            anyhow!("通知設定ファイル {path} の読み込みに失敗しました: {error}")
        }
        dotenvy::Error::EnvVar(error) => {
            anyhow!("通知設定ファイル {path} の環境変数展開に失敗しました: {error}")
        }
        _ => anyhow!("通知設定ファイル {path} の読み込みに失敗しました"),
    }
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
            watched_points: parse_watched_points(
                &get("WATCHED_POINTS").unwrap_or_else(|| "[]".to_string()),
            )?,
            region_min_scales,
            other_min_scale,
            attach_map,
            tile_url_template,
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

fn parse_watched_points(raw: &str) -> Result<Vec<WatchedPoint>> {
    let mut points: Vec<WatchedPoint> = serde_json::from_str(raw)
        .context("WATCHED_POINTS は pref・name・任意の label を持つ JSON 配列で指定してください")?;
    for (i, point) in points.iter_mut().enumerate() {
        point.pref = point.pref.trim().to_string();
        point.name = point.name.trim().to_string();
        point.label = point.label.trim().to_string();
        ensure!(
            !point.pref.is_empty() && !point.name.is_empty(),
            "WATCHED_POINTS の {} 番目: pref と name は必須です",
            i + 1
        );
        for value in [&point.pref, &point.name, &point.label] {
            ensure!(
                value.chars().count() <= 100 && !value.chars().any(char::is_control),
                "WATCHED_POINTS の {} 番目: 各項目は改行なしの100文字以内で指定してください",
                i + 1
            );
        }
    }
    // 登録順を維持し、まったく同じ設定の重複を除く。
    let mut unique = Vec::new();
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
    use std::path::{Component, Path};

    fn lookup(values: &[(&str, &str)], key: &str) -> Option<String> {
        values
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string())
    }

    fn config(env: &[(&str, &str)], files: &[(&str, &[(&str, &str)])]) -> Result<Config> {
        Config::from_sources_with_resolver(
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
            |path| {
                let mut normalized = PathBuf::new();
                for component in Path::new(path).components() {
                    match component {
                        Component::CurDir => {}
                        Component::ParentDir => {
                            normalized.pop();
                        }
                        component => normalized.push(component.as_os_str()),
                    }
                }
                Ok(normalized)
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
    fn watched_points_parse_and_deduplicate() {
        let points = parse_watched_points(
            r#"[
            {"pref":" 東京都 ","name":"東京都23区","label":" 自宅周辺 "},
            {"pref":"東京都","name":"東京都23区","label":"自宅周辺"},
            {"pref":"神奈川県","name":"神奈川県東部"}
        ]"#,
        )
        .unwrap();
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].pref, "東京都");
        assert_eq!(points[0].label, "自宅周辺");
        assert_eq!(points[1].label, "");
        assert!(parse_watched_points("[]").unwrap().is_empty());
    }

    #[test]
    fn invalid_watched_points_are_rejected() {
        for raw in [
            "",
            "{}",
            r#"[{"pref":"東京都"}]"#,
            r#"[{"pref":" ","name":"東京都23区"}]"#,
            r#"[{"pref":"東京都","name":"東京都23区","lable":"自宅"}]"#,
            r#"[{"pref":"東京都","name":"東京都23区","label":"自宅\n職場"}]"#,
        ] {
            assert!(parse_watched_points(raw).is_err(), "{raw}");
        }
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
                        (
                            "WATCHED_POINTS",
                            r#"[{"pref":"東京都","name":"東京都23区","label":"自宅"}]"#,
                        ),
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
                        (
                            "WATCHED_POINTS",
                            r#"[{"pref":"大阪府","name":"大阪府北部","label":"職場"}]"#,
                        ),
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
        assert_eq!(first.watched_points[0].label, "自宅");
        assert_eq!(second.watched_points[0].pref, "大阪府");
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

    #[test]
    fn canonical_path_aliases_are_rejected_as_duplicates() {
        let root =
            std::env::temp_dir().join(format!("quake-alert-config-alias-{}", std::process::id()));
        let nested = root.join("nested");
        let file = root.join("foo.env");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(&file, "DISCORD_WEBHOOK_URL=https://example.com/hook\n").unwrap();
        let paths = format!("{},{}", file.display(), nested.join("../foo.env").display());
        let error = Config::from_sources(
            |key| (key == "NOTIFICATION_CONFIG_FILES").then_some(paths.clone()),
            read_notification_file,
        )
        .unwrap_err();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(error.to_string().contains("重複しています"));
    }

    #[test]
    fn dotenv_syntax_errors_do_not_include_secret_values() {
        let path = std::env::temp_dir().join(format!(
            "quake-alert-config-secret-{}.env",
            std::process::id()
        ));
        let secret = "https://discord.com/api/webhooks/private-secret";
        std::fs::write(
            &path,
            format!("DISCORD_WEBHOOK_URL=\"{secret}\nOTHER_MIN_SCALE=40\n"),
        )
        .unwrap();
        let error = read_notification_file(path.to_str().unwrap()).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("構文エラー"));
        assert!(message.contains("行内位置"));
        assert!(message.contains(path.to_str().unwrap()));
        assert!(!message.contains(secret));
    }

    #[test]
    fn dotenv_values_do_not_expand_process_environment() {
        let path = std::env::temp_dir().join(format!(
            "quake-alert-config-expansion-{}.env",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "# don't expand variables\nDISCORD_WEBHOOK_URL=$HOME\n# a \"comment\"\nTILE_URL_TEMPLATE='$HOME'\n",
        )
        .unwrap();
        let values = read_notification_file(path.to_str().unwrap()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(values["DISCORD_WEBHOOK_URL"], "$HOME");
        assert_eq!(values["TILE_URL_TEMPLATE"], "$HOME");
    }
}
