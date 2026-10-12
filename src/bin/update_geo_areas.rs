//! 気象庁の区域名称表とGISから、予報区判定に使うRustデータを照合・更新する。

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, ensure, Context, Result};

#[path = "../geo_area_data.rs"]
mod bundled_areas;
#[path = "../geo_area_centers.rs"]
mod bundled_centers;

const AREA_SOURCE: &str = "https://www.jma.go.jp/jma/kishou/know/jishin/joho/shindo-name.html";
const GIS_SOURCE: &str =
    "https://www.data.jma.go.jp/developer/gis/20240520_AreaForecastLocalE_GIS.zip";
const AREA_OUTPUT: &str = "src/geo_area_data.rs";
const CENTER_OUTPUT: &str = "src/geo_area_centers.rs";

type Municipality = (String, String, String);
type Center = (String, String, f64, f64);

struct Inputs {
    area_html: PathBuf,
    gis_shp: PathBuf,
    gis_dbf: PathBuf,
    write: bool,
}

fn usage() -> &'static str {
    "使い方: cargo run --bin update_geo_areas -- --area-html HTML --gis-shp SHP --gis-dbf DBF [--write]\n既定では公式データと組み込みデータを照合します。--writeでRustデータを更新します。"
}

fn arguments() -> Result<Inputs> {
    let mut area_html = None;
    let mut gis_shp = None;
    let mut gis_dbf = None;
    let mut write = false;
    let mut args = env::args().skip(1);
    while let Some(option) = args.next() {
        let destination = match option.as_str() {
            "--area-html" => &mut area_html,
            "--gis-shp" => &mut gis_shp,
            "--gis-dbf" => &mut gis_dbf,
            "--write" => {
                write = true;
                continue;
            }
            "--help" | "-h" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            _ => bail!("不明な引数: {option}\n{}", usage()),
        };
        *destination = Some(PathBuf::from(
            args.next()
                .ok_or_else(|| anyhow!("{option}のパスがありません"))?,
        ));
    }
    Ok(Inputs {
        area_html: area_html.ok_or_else(|| anyhow!("--area-htmlが必要です\n{}", usage()))?,
        gis_shp: gis_shp.ok_or_else(|| anyhow!("--gis-shpが必要です\n{}", usage()))?,
        gis_dbf: gis_dbf.ok_or_else(|| anyhow!("--gis-dbfが必要です\n{}", usage()))?,
        write,
    })
}

fn normalize_area_name(name: &str) -> String {
    name.chars()
        .map(|character| match character {
            '０'..='９' => char::from_u32(character as u32 - '０' as u32 + '0' as u32)
                .expect("全角数字を変換できません"),
            'Ａ'..='Ｚ' => char::from_u32(character as u32 - 'Ａ' as u32 + 'A' as u32)
                .expect("全角英字を変換できません"),
            'ａ'..='ｚ' => char::from_u32(character as u32 - 'ａ' as u32 + 'a' as u32)
                .expect("全角英字を変換できません"),
            other => other,
        })
        .collect()
}

fn rowspan(tag: &str) -> Result<usize> {
    let Some((_, rest)) = tag.split_once("rowspan") else {
        return Ok(1);
    };
    let rest = rest
        .trim_start()
        .strip_prefix('=')
        .ok_or_else(|| anyhow!("rowspan属性が不正です: {tag}"))?
        .trim_start();
    let rest = rest
        .strip_prefix('"')
        .or_else(|| rest.strip_prefix('\''))
        .unwrap_or(rest);
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let count: usize = digits.parse().context("rowspan属性が数値ではありません")?;
    ensure!(count > 0, "rowspan属性は1以上である必要があります");
    Ok(count)
}

fn resolve_row(
    raw: Vec<(String, usize)>,
    pending: &mut BTreeMap<usize, (String, usize)>,
) -> Result<[String; 4]> {
    let mut raw = raw.into_iter();
    let mut resolved = Vec::with_capacity(4);
    for column in 0..4 {
        if let Some((text, remaining)) = pending.remove(&column) {
            resolved.push(text.clone());
            if remaining > 1 {
                pending.insert(column, (text, remaining - 1));
            }
            continue;
        }
        let (text, span) = raw
            .next()
            .ok_or_else(|| anyhow!("区域名称表の行は4列必要です: {resolved:?}"))?;
        if span > 1 {
            pending.insert(column, (text.clone(), span - 1));
        }
        resolved.push(text);
    }
    ensure!(raw.next().is_none(), "区域名称表に余分な列があります");
    resolved.try_into().map_err(|_| anyhow!("列数が不正です"))
}

fn parse_area_html(html: &str) -> Result<(Vec<Municipality>, BTreeMap<String, String>)> {
    let start = html
        .find("<table")
        .ok_or_else(|| anyhow!("区域名称表にtableがありません"))?;
    let end = html[start..]
        .find("</table>")
        .ok_or_else(|| anyhow!("区域名称表のtableが閉じられていません"))?
        + start;
    let html = &html[start..end];
    let mut position = 0;
    let mut row = None::<Vec<(String, usize)>>;
    let mut cell = None::<(String, usize)>;
    let mut pending = BTreeMap::new();
    let mut municipalities = BTreeSet::new();
    let mut area_pref = BTreeMap::new();

    while let Some(open) = html[position..].find('<') {
        let open = position + open;
        if let Some((text, _)) = cell.as_mut() {
            text.push_str(&html[position..open]);
        }
        let close = html[open..]
            .find('>')
            .ok_or_else(|| anyhow!("区域名称表に閉じられていないタグがあります"))?
            + open;
        let tag = html[open + 1..close].trim();
        if tag == "tr" {
            ensure!(row.is_none(), "trが入れ子になっています");
            row = Some(Vec::new());
        } else if tag == "/tr" {
            let raw = row
                .take()
                .ok_or_else(|| anyhow!("対応するtrがありません"))?;
            if raw.first().is_some_and(|(text, _)| text == "都道府県名") {
                position = close + 1;
                continue;
            }
            let [pref, _, area, names] = resolve_row(raw, &mut pending)?;
            let normalized_area = normalize_area_name(&area);
            if let Some(previous_pref) = area_pref.insert(normalized_area, pref.clone()) {
                ensure!(
                    previous_pref == pref,
                    "同じ区域名が複数県に存在します: {area}"
                );
            }
            for municipality in names.split('、') {
                ensure!(!municipality.is_empty(), "市区町村名が空です: {area}");
                ensure!(
                    municipalities.insert((pref.clone(), municipality.to_string(), area.clone())),
                    "市区町村が重複しています: {pref} {municipality}"
                );
            }
        } else if tag.starts_with("td") || tag.starts_with("th") {
            if row.is_some() {
                ensure!(cell.is_none(), "セルが入れ子になっています");
                cell = Some((String::new(), rowspan(tag)?));
            }
        } else if tag == "/td" || tag == "/th" {
            if let Some((text, span)) = cell.take() {
                let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                ensure!(
                    !text.contains('&'),
                    "区域名称表に未対応のHTML実体があります: {text}"
                );
                row.as_mut()
                    .ok_or_else(|| anyhow!("対応するtrがありません"))?
                    .push((text, span));
            }
        }
        position = close + 1;
    }

    ensure!(!municipalities.is_empty(), "市区町村の対応がありません");
    ensure!(
        pending.is_empty(),
        "区域名称表のrowspanが途中で終わっています"
    );
    let mut municipalities: Vec<_> = municipalities.into_iter().collect();
    municipalities.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| right.1.chars().count().cmp(&left.1.chars().count()))
            .then_with(|| left.1.cmp(&right.1))
    });
    Ok((municipalities, area_pref))
}

fn le_u16(data: &[u8], offset: usize) -> Result<usize> {
    let bytes: [u8; 2] = data
        .get(offset..offset + 2)
        .ok_or_else(|| anyhow!("GISデータが途中で終わっています"))?
        .try_into()?;
    Ok(u16::from_le_bytes(bytes) as usize)
}

fn le_u32(data: &[u8], offset: usize) -> Result<usize> {
    let bytes: [u8; 4] = data
        .get(offset..offset + 4)
        .ok_or_else(|| anyhow!("GISデータが途中で終わっています"))?
        .try_into()?;
    Ok(u32::from_le_bytes(bytes) as usize)
}

fn le_f64(data: &[u8], offset: usize) -> Result<f64> {
    let bytes: [u8; 8] = data
        .get(offset..offset + 8)
        .ok_or_else(|| anyhow!("GISデータが途中で終わっています"))?
        .try_into()?;
    Ok(f64::from_le_bytes(bytes))
}

fn dbf_names(path: &Path) -> Result<Vec<String>> {
    let data = fs::read(path).with_context(|| format!("DBFを読めません: {}", path.display()))?;
    ensure!(data.len() >= 33, "DBFヘッダーが不正です");
    let record_count = le_u32(&data, 4)?;
    let header_length = le_u16(&data, 8)?;
    let record_length = le_u16(&data, 10)?;
    let mut offset = 1;
    let mut field_position = 32;
    let mut name_field = None;
    while data.get(field_position) != Some(&0x0d) {
        let field = data
            .get(field_position..field_position + 32)
            .ok_or_else(|| anyhow!("DBFのフィールド定義が不正です"))?;
        let name_end = field[..11].iter().position(|byte| *byte == 0).unwrap_or(11);
        let name = std::str::from_utf8(&field[..name_end])?;
        let length = field[16] as usize;
        if name == "name" {
            name_field = Some((offset, length));
        }
        offset += length;
        field_position += 32;
    }
    let (name_offset, name_length) =
        name_field.ok_or_else(|| anyhow!("DBFにname列がありません"))?;
    ensure!(
        name_offset + name_length <= record_length,
        "DBFのname列が範囲外です"
    );
    ensure!(field_position < header_length, "DBFのヘッダー長が不正です");
    let mut names = Vec::with_capacity(record_count);
    for index in 0..record_count {
        let start = header_length + index * record_length + name_offset;
        let bytes = data
            .get(start..start + name_length)
            .ok_or_else(|| anyhow!("DBFのレコードが途中で終わっています: {index}"))?;
        names.push(std::str::from_utf8(bytes)?.trim().to_string());
    }
    Ok(names)
}

fn area_centers(
    shp_path: &Path,
    dbf_path: &Path,
    area_pref: &BTreeMap<String, String>,
) -> Result<Vec<Center>> {
    let names = dbf_names(dbf_path)?;
    let source =
        File::open(shp_path).with_context(|| format!("SHPを読めません: {}", shp_path.display()))?;
    let mut source = BufReader::new(source);
    let mut file_header = [0_u8; 100];
    source.read_exact(&mut file_header)?;
    ensure!(
        u32::from_be_bytes(file_header[..4].try_into()?) == 9994,
        "SHPのヘッダーが不正です"
    );
    let mut centers = BTreeMap::new();
    let mut shape_index = 0;
    loop {
        let mut record_header = [0_u8; 8];
        if source.read(&mut record_header[..1])? == 0 {
            break;
        }
        source.read_exact(&mut record_header[1..])?;
        let content_words = u32::from_be_bytes(record_header[4..].try_into()?) as usize;
        let content_length = content_words
            .checked_mul(2)
            .ok_or_else(|| anyhow!("SHPレコード長が不正です"))?;
        ensure!(
            content_length <= 128 * 1024 * 1024,
            "SHPレコードが大きすぎます"
        );
        let mut content = vec![0_u8; content_length];
        source.read_exact(&mut content)?;
        let shape_type = le_u32(&content, 0)?;
        if shape_type == 5 {
            let part_count = le_u32(&content, 36)?;
            let point_count = le_u32(&content, 40)?;
            ensure!(point_count > 0, "SHPのポリゴンに点がありません");
            let points_offset = 44 + part_count * 4;
            ensure!(
                points_offset + point_count * 16 <= content.len(),
                "SHPのポリゴンが途中で終わっています"
            );
            let mut latitude = 0.0;
            let mut longitude = 0.0;
            for point in 0..point_count {
                longitude += le_f64(&content, points_offset + point * 16)?;
                latitude += le_f64(&content, points_offset + point * 16 + 8)?;
            }
            let area = names
                .get(shape_index)
                .ok_or_else(|| anyhow!("SHPの件数がDBFより多いです"))?;
            if let Some(pref) = area_pref.get(&normalize_area_name(area)) {
                ensure!(
                    centers
                        .insert(
                            (pref.clone(), area.clone()),
                            (
                                latitude / point_count as f64,
                                longitude / point_count as f64
                            )
                        )
                        .is_none(),
                    "GISの区域が重複しています: {area}"
                );
            }
        } else {
            ensure!(shape_type == 0, "未対応のSHP形状です: {shape_type}");
        }
        shape_index += 1;
    }
    ensure!(
        shape_index == names.len(),
        "SHPとDBFのレコード件数が異なります"
    );
    ensure!(
        centers.len() == area_pref.len(),
        "区域名称表とGISの区域が一致しません"
    );
    Ok(centers
        .into_iter()
        .map(|((pref, area), (latitude, longitude))| (area, pref, latitude, longitude))
        .collect())
}

fn coordinate(value: f64) -> String {
    let mut value = format!("{value:.6}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value
}

fn check(areas: &[Municipality], centers: &[Center]) -> Result<()> {
    let bundled_areas: BTreeSet<_> = bundled_areas::EEW_AREA_MUNICIPALITIES
        .iter()
        .map(|(pref, municipality, area)| {
            (pref.to_string(), municipality.to_string(), area.to_string())
        })
        .collect();
    let source_areas: BTreeSet<_> = areas.iter().cloned().collect();
    if bundled_areas != source_areas {
        let missing: Vec<_> = source_areas.difference(&bundled_areas).take(5).collect();
        let extra: Vec<_> = bundled_areas.difference(&source_areas).take(5).collect();
        bail!("市区町村・区域対応が公式表と異なります。未収録: {missing:?} / 余分: {extra:?}");
    }
    let bundled_centers: BTreeSet<_> = bundled_centers::EEW_AREA_CENTERS
        .iter()
        .map(|(area, pref, latitude, longitude)| {
            (
                pref.to_string(),
                area.to_string(),
                coordinate(*latitude),
                coordinate(*longitude),
            )
        })
        .collect();
    let source_centers: BTreeSet<_> = centers
        .iter()
        .map(|(area, pref, latitude, longitude)| {
            (
                pref.clone(),
                area.clone(),
                coordinate(*latitude),
                coordinate(*longitude),
            )
        })
        .collect();
    if bundled_centers != source_centers {
        let missing: Vec<_> = source_centers
            .difference(&bundled_centers)
            .take(5)
            .collect();
        let extra: Vec<_> = bundled_centers
            .difference(&source_centers)
            .take(5)
            .collect();
        bail!("区域代表座標が公式GISと異なります。未収録: {missing:?} / 余分: {extra:?}");
    }
    Ok(())
}

fn render_areas(areas: &[Municipality]) -> String {
    let mut output = format!(
        "// JMA “緊急地震速報や震度情報で用いる区域の名称”を収録した静的データ。\n// Source: {AREA_SOURCE}\n// Each tuple is (都道府県名, 市区町村名, 緊急地震速報の細分区域名).\npub(crate) const EEW_AREA_MUNICIPALITIES: &[(&str, &str, &str)] = &[\n"
    );
    for (pref, municipality, area) in areas {
        output.push_str(&format!("    ({pref:?}, {municipality:?}, {area:?}),\n"));
    }
    output.push_str("];\n");
    output
}

fn render_centers(centers: &[Center]) -> String {
    let mut output = format!(
        "// JMA AreaForecastLocalE GIS由来の区域代表座標を収録した静的データ。\n// Source: {GIS_SOURCE}\n// Each tuple is (細分区域名, 都道府県名, 代表緯度, 代表経度).\npub(crate) const EEW_AREA_CENTERS: &[(&str, &str, f64, f64)] = &[\n"
    );
    for (area, pref, latitude, longitude) in centers {
        output.push_str(&format!(
            "    ({area:?}, {pref:?}, {}, {}),\n",
            coordinate(*latitude),
            coordinate(*longitude)
        ));
    }
    output.push_str("];\n");
    output
}

fn main() -> Result<()> {
    let args = arguments()?;
    let html = fs::read_to_string(&args.area_html)
        .with_context(|| format!("区域名称表を読めません: {}", args.area_html.display()))?;
    let (areas, area_pref) = parse_area_html(&html)?;
    let centers = area_centers(&args.gis_shp, &args.gis_dbf, &area_pref)?;
    if args.write {
        fs::write(AREA_OUTPUT, render_areas(&areas))?;
        fs::write(CENTER_OUTPUT, render_centers(&centers))?;
        println!(
            "区域データを更新しました: 市区町村{}件、区域{}件",
            areas.len(),
            centers.len()
        );
    } else {
        check(&areas, &centers)?;
        println!(
            "公式データと一致しました: 市区町村{}件、区域{}件",
            areas.len(),
            centers.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rowspan_and_municipalities() {
        let html = "<table><tr><th>都道府県名</th><th>予報区</th><th>区域名</th><th>市区町村</th></tr><tr><td rowspan=\"2\">北海道</td><td rowspan=\"2\">道央</td><td>石狩地方北部</td><td>石狩市、当別町</td></tr><tr><td>石狩地方南部</td><td>千歳市</td></tr></table>";
        let (areas, area_pref) = parse_area_html(html).unwrap();
        assert_eq!(areas.len(), 3);
        assert!(areas.contains(&("北海道".into(), "千歳市".into(), "石狩地方南部".into())));
        assert_eq!(area_pref["石狩地方北部"], "北海道");
    }

    #[test]
    fn normalizes_fullwidth_area_digits() {
        assert_eq!(normalize_area_name("東京都２３区"), "東京都23区");
    }
}
