//! P2P地震情報の551履歴と組み込み観測点TSVの名称一致を監査する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

const DEFAULT_STATIONS: &str = "src/data/observation_stations.tsv";

#[derive(Default)]
struct Summary {
    reports: usize,
    points: usize,
    area_count: usize,
    names: HashMap<(String, String), usize>,
    area_names: HashSet<(String, String)>,
}

fn load_stations(path: &Path) -> Result<HashSet<(String, String)>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("TSVを読み込めません: {}", path.display()))?;
    let mut stations = HashSet::new();
    for line in contents.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<_> = line.split('\t').collect();
        if columns.len() != 4 {
            bail!("TSVの列数が不正です: {line}");
        }
        stations.insert((columns[0].to_string(), columns[1].to_string()));
    }
    Ok(stations)
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn audit(reports: &[Value]) -> BTreeMap<String, Summary> {
    let mut summary = BTreeMap::new();
    for report in reports {
        let issue_type = report
            .get("issue")
            .map(|issue| string_field(issue, "type"))
            .unwrap_or_default()
            .to_string();
        let current = summary.entry(issue_type).or_insert_with(Summary::default);
        current.reports += 1;
        let Some(points) = report.get("points").and_then(Value::as_array) else {
            continue;
        };
        for point in points {
            current.points += 1;
            let key = (
                string_field(point, "pref").to_string(),
                string_field(point, "addr").to_string(),
            );
            if point
                .get("isArea")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                current.area_count += 1;
                current.area_names.insert(key);
            } else {
                *current.names.entry(key).or_insert(0) += 1;
            }
        }
    }
    summary
}

fn print_summary(summary: BTreeMap<String, Summary>, stations: &HashSet<(String, String)>) {
    for (issue_type, values) in summary {
        let matched_unique = values
            .names
            .keys()
            .filter(|(pref, name)| {
                !name.is_empty() && stations.contains(&(pref.clone(), name.clone()))
            })
            .count();
        let mut missing: Vec<_> = values
            .names
            .iter()
            .filter(|(key, _)| !stations.contains(key))
            .map(|((pref, name), count)| (pref.as_str(), name.as_str(), *count))
            .collect();
        missing.sort_unstable();

        println!(
            "{}: 報数={} 地点出現数={} 固有地点={} isArea={} 区域固有名={} 観測点一致={} 観測点未一致={}",
            if issue_type.is_empty() {
                "(不明)"
            } else {
                &issue_type
            },
            values.reports,
            values.points,
            values.names.len(),
            values.area_count,
            values.area_names.len(),
            matched_unique,
            missing.len()
        );
        for (pref, name, count) in missing {
            println!("  未一致 {pref}\t{name}\t{count}件");
        }
    }
}

fn print_help() {
    println!(
        "使い方: cargo run --bin audit_p2p_points -- <履歴JSON> [--stations TSV]\n\
         履歴JSONは /history?codes=551 のJSON配列を保存したファイルです。\n\
         --stations の既定値: {DEFAULT_STATIONS}"
    );
}

fn parse_args() -> Result<Option<(PathBuf, PathBuf)>> {
    let mut args = env::args().skip(1);
    let Some(input) = args.next() else {
        return Ok(None);
    };
    if matches!(input.as_str(), "-h" | "--help") {
        return Ok(None);
    }
    let mut stations = PathBuf::from(DEFAULT_STATIONS);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--stations" => {
                stations = PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--stations の値を指定してください"))?,
                );
            }
            _ => bail!("未知の引数です: {argument}"),
        }
    }
    Ok(Some((PathBuf::from(input), stations)))
}

fn main() -> Result<()> {
    let Some((input, station_path)) = parse_args()? else {
        print_help();
        return Ok(());
    };
    let reports: Vec<Value> = serde_json::from_str(
        &fs::read_to_string(&input)
            .with_context(|| format!("履歴JSONを読み込めません: {}", input.display()))?,
    )
    .context("履歴JSONを解析できません")?;
    let stations = load_stations(&station_path)?;
    print_summary(audit(&reports), &stations);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn area_and_station_names_are_counted_separately() {
        let reports = vec![
            json!({
                "issue":{"type":"ScalePrompt"},
                "points":[
                    {"pref":"東京","addr":"東京都２３区","isArea":true}
                ]
            }),
            json!({
                "issue":{"type":"DetailScale"},
                "points":[
                    {"pref":"東京都","addr":"東京都23区","isArea":false},
                    {"pref":"東京都","addr":"東京都23区","isArea":false},
                    {"pref":"千葉県","addr":"存在しない地点","isArea":false}
                ]
            }),
        ];
        let stations = HashSet::from([("東京都".to_string(), "東京都23区".to_string())]);
        let summary = audit(&reports);

        let prompt = &summary["ScalePrompt"];
        assert_eq!(prompt.area_count, 1);
        assert_eq!(prompt.area_names.len(), 1);
        assert_eq!(prompt.names.len(), 0);

        let detail = &summary["DetailScale"];
        assert_eq!(detail.names.len(), 2);
        assert_eq!(
            detail
                .names
                .keys()
                .filter(|key| stations.contains(*key))
                .count(),
            1
        );
    }
}
