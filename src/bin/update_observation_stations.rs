//! 気象庁の公開観測点JSONを検証し、botに組み込むTSVを生成する。

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

const SOURCE: &str = "https://ds.data.jma.go.jp/eqev/data/intens-st/stations.json";
const DEFAULT_OUTPUT: &str = "src/data/observation_stations.tsv";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const PREFS: &[&str] = &[
    "北海道",
    "青森県",
    "岩手県",
    "宮城県",
    "秋田県",
    "山形県",
    "福島県",
    "茨城県",
    "栃木県",
    "群馬県",
    "埼玉県",
    "千葉県",
    "東京都",
    "神奈川県",
    "新潟県",
    "富山県",
    "石川県",
    "福井県",
    "山梨県",
    "長野県",
    "岐阜県",
    "静岡県",
    "愛知県",
    "三重県",
    "滋賀県",
    "京都府",
    "大阪府",
    "兵庫県",
    "奈良県",
    "和歌山県",
    "鳥取県",
    "島根県",
    "岡山県",
    "広島県",
    "山口県",
    "徳島県",
    "香川県",
    "愛媛県",
    "高知県",
    "福岡県",
    "佐賀県",
    "長崎県",
    "熊本県",
    "大分県",
    "宮崎県",
    "鹿児島県",
    "沖縄県",
];

fn integer_field(station: &Value, key: &str) -> Result<i32> {
    let value = station
        .get(key)
        .ok_or_else(|| anyhow!("観測点に {key} がありません"))?;
    match value {
        Value::Number(value) => value
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| anyhow!("{key} が整数ではありません")),
        Value::String(value) => value
            .parse()
            .with_context(|| format!("{key} が整数ではありません")),
        _ => Err(anyhow!("{key} が整数ではありません")),
    }
}

fn float_field(station: &Value, key: &str) -> Result<f64> {
    let value = station
        .get(key)
        .ok_or_else(|| anyhow!("観測点に {key} がありません"))?;
    match value {
        Value::Number(value) => value
            .as_f64()
            .ok_or_else(|| anyhow!("{key} が数値ではありません")),
        Value::String(value) => value
            .parse()
            .with_context(|| format!("{key} が数値ではありません")),
        _ => Err(anyhow!("{key} が数値ではありません")),
    }
}

fn format_coord(value: f64) -> String {
    let mut formatted = format!("{value:.6}");
    while formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    if formatted == "-0" {
        formatted.clear();
        formatted.push('0');
    }
    formatted
}

/// 重複・不正値は書き込み前に拒否し、登録順によらない出力にする。
fn convert(stations: &[Value]) -> Result<Vec<String>> {
    if stations.is_empty() {
        bail!("観測点データは空でない配列である必要があります");
    }

    let mut rows = BTreeMap::new();
    for station in stations {
        let station = station
            .as_object()
            .ok_or_else(|| anyhow!("観測点がオブジェクトではありません"))?;
        let station = Value::Object(station.clone());
        let pref_code = integer_field(&station, "pref")?;
        let affiliation = integer_field(&station, "affi")?;
        let name = station
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("観測点名が文字列ではありません"))?;
        let latitude = float_field(&station, "lat")?;
        let longitude = float_field(&station, "lon")?;

        if !(1..=47).contains(&pref_code) || !matches!(affiliation, 0..=2) {
            bail!("不正な都道府県コード・所属です");
        }
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            bail!("不正な観測点名です");
        }
        if !(latitude.is_finite()
            && longitude.is_finite()
            && (-90.0..=90.0).contains(&latitude)
            && (-180.0..=180.0).contains(&longitude)
            && (latitude, longitude) != (0.0, 0.0))
        {
            bail!("不正な座標: {name}");
        }

        let key = (pref_code, name.to_string());
        if rows.insert(key, (latitude, longitude)).is_some() {
            bail!("都道府県・観測点名の重複: {pref_code} {name}");
        }
    }

    rows.into_iter()
        .map(|((pref_code, name), (latitude, longitude))| {
            let pref = PREFS
                .get(usize::try_from(pref_code - 1).expect("都道府県コードが負です"))
                .ok_or_else(|| anyhow!("不正な都道府県コード: {pref_code}"))?;
            Ok(format!(
                "{pref}\t{name}\t{}\t{}",
                format_coord(latitude),
                format_coord(longitude)
            ))
        })
        .collect()
}

/// UNIX時刻の日数をグレゴリオ暦へ変換する（UTC）。外部の日付ライブラリを不要にする。
fn utc_date() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86_400;
    let z = i64::try_from(days).unwrap_or(i64::MAX) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let month_part = (5 * doy + 2) / 153;
    let day = doy - (153 * month_part + 2) / 5 + 1;
    let month = month_part + if month_part < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn temporary_path(output: &Path) -> PathBuf {
    let mut temporary = output.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

#[cfg(not(windows))]
fn replace_output(temporary: &Path, output: &Path) -> Result<()> {
    fs::rename(temporary, output)
        .with_context(|| format!("生成したTSVを置き換えられません: {}", output.display()))?;
    Ok(())
}

#[cfg(windows)]
fn replace_output(temporary: &Path, output: &Path) -> Result<()> {
    // Windowsのrenameは既存ファイルを上書きしないため、既存TSVを一時退避してから
    // 生成物を移動する。2回目のrenameに失敗した場合は、元のTSVを復元する。
    if !output.exists() {
        fs::rename(temporary, output)
            .with_context(|| format!("生成したTSVを置き換えられません: {}", output.display()))?;
        return Ok(());
    }

    let mut backup = output.as_os_str().to_os_string();
    backup.push(".bak");
    let backup = PathBuf::from(backup);
    if backup.exists() {
        fs::remove_file(&backup)
            .with_context(|| format!("既存のバックアップを削除できません: {}", backup.display()))?;
    }
    fs::rename(output, &backup)
        .with_context(|| format!("既存のTSVを退避できません: {}", output.display()))?;

    match fs::rename(temporary, output) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(error) => match fs::rename(&backup, output) {
            Ok(()) => Err(error).with_context(|| {
                format!("生成したTSVを置き換えられません: {}", output.display())
            }),
            Err(restore_error) => Err(anyhow!(
                "生成したTSVを置き換えられず、既存TSVの復元にも失敗しました: 置換={error}; 復元={restore_error}"
            )),
        },
    }
}

fn print_help() {
    println!(
        "使い方: cargo run --bin update_observation_stations -- [--input JSON] [--output TSV]\n"
    );
    println!("--input を省略すると、公開JSONを取得します。");
    println!("--output の既定値: {DEFAULT_OUTPUT}");
}

fn parse_args() -> Result<Option<(Option<PathBuf>, PathBuf)>> {
    let mut input = None;
    let mut output = PathBuf::from(DEFAULT_OUTPUT);
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--input" => {
                input = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--input の値を指定してください"))?,
                ));
            }
            "--output" => {
                output = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--output の値を指定してください"))?,
                );
            }
            _ => bail!("未知の引数です: {argument}"),
        }
    }
    Ok(Some((input, output)))
}

#[tokio::main]
async fn main() -> Result<()> {
    let Some((input, output)) = parse_args()? else {
        print_help();
        return Ok(());
    };

    let raw = if let Some(input) = input {
        fs::read(&input)
            .with_context(|| format!("入力JSONを読み込めません: {}", input.display()))?
    } else {
        reqwest::Client::builder()
            .user_agent("quake-alert-bot-observation-stations/1")
            .timeout(HTTP_TIMEOUT)
            .build()?
            .get(SOURCE)
            .send()
            .await
            .context("観測点JSONを取得できません")?
            .error_for_status()
            .context("観測点JSONのHTTP応答が失敗しました")?
            .bytes()
            .await
            .context("観測点JSONの本文を読み込めません")?
            .to_vec()
    };
    let stations: Vec<Value> =
        serde_json::from_slice(&raw).context("観測点JSONを解析できません")?;
    let lines = convert(&stations)?;
    let header = format!(
        "# 出典: 気象庁 震度観測点 {SOURCE}\n# quake-alert-botが都道府県名へ変換・整列（座標は公開JSONの値を使用）\n# 生成日(UTC): {} / 入力SHA-256: {:x}\n# 観測点数: {}\n# 都道府県\t観測点名\t緯度\t経度\n",
        utc_date(),
        Sha256::digest(&raw),
        lines.len()
    );
    let contents = format!("{header}{}\n", lines.join("\n"));
    let temporary = temporary_path(&output);
    fs::write(&temporary, contents)
        .with_context(|| format!("一時TSVを書き込めません: {}", temporary.display()))?;
    replace_output(&temporary, &output)?;
    println!("{}地点を保存しました: {}", lines.len(), output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn station(pref: &str, affi: &str, name: &str, lat: &str, lon: &str) -> Value {
        json!({"pref":pref,"affi":affi,"name":name,"lat":lat,"lon":lon})
    }

    #[test]
    fn uses_public_json_coordinates() {
        assert_eq!(
            convert(&[station("1", "0", "観測点", "43.17167", "141.315")]).unwrap(),
            vec!["北海道\t観測点\t43.17167\t141.315"]
        );
        assert_eq!(
            convert(&[station("1", "0", "観測点", "43.17", "141.32")]).unwrap(),
            vec!["北海道\t観測点\t43.17\t141.32"]
        );
    }

    #[test]
    fn formats_binary_float_without_artifacts() {
        assert_eq!(
            convert(&[station("1", "0", "観測点", "35.69", "139.70")]).unwrap(),
            vec!["北海道\t観測点\t35.69\t139.7"]
        );
    }

    #[test]
    fn invalid_coordinates_and_keys_are_rejected() {
        for value in [
            station("1", "0", "観測点", "nan", "141.32"),
            station("1", "0", "観測点", "43.17", "inf"),
            station("1", "0", "観測点", "91", "141.32"),
            station("1", "0", "観測点", "43.17", "181"),
            station("0", "0", "観測点", "43.17", "141.32"),
            station("48", "0", "観測点", "43.17", "141.32"),
            station("1", "3", "観測点", "43.17", "141.32"),
            station("1", "0", "地点\t名前", "43.17", "141.32"),
        ] {
            assert!(convert(&[value]).is_err());
        }
        assert!(convert(&[
            station("1", "0", "観測点", "43.17", "141.32"),
            station("1", "0", "観測点", "43.17", "141.32"),
        ])
        .is_err());
        assert!(convert(&[]).is_err());
    }

    #[test]
    fn same_name_in_different_prefectures_is_kept() {
        let rows = convert(&[
            station("2", "0", "同名", "43.17", "141.32"),
            station("1", "0", "同名", "43.17", "141.32"),
        ])
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with("北海道\t"));
        assert!(rows[1].starts_with("青森県\t"));
    }

    #[test]
    fn replaces_existing_output_file() {
        let directory = env::temp_dir().join(format!(
            "quake-alert-bot-update-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join("stations.tsv");
        let temporary = temporary_path(&output);
        fs::write(&output, "old\n").unwrap();
        fs::write(&temporary, "new\n").unwrap();

        replace_output(&temporary, &output).unwrap();

        assert_eq!(fs::read_to_string(&output).unwrap(), "new\n");
        assert!(!temporary.exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
