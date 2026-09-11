//! 震源地・各地の震度をプロットした地図画像(WebP)の生成。
//!
//! staticmap クレートで地図タイル(既定は国土地理院の白地図)を取得し、震源地（黒縁）と
//! 各観測地点（白縁、震度に応じた色）に二重円マーカーを描く。
//! staticmap は PNG しか出力できないため、生成後に WebP へ再エンコードして軽量化する。

use std::f64::consts::PI;

use anyhow::{anyhow, Result};
use staticmap::{
    tools::{Circle, CircleBuilder, Color},
    StaticMapBuilder,
};

use crate::intensity::marker_rgb;

/// WebP ロスレス圧縮の努力度（0.0〜100.0、高いほど高圧縮）。
/// 地図は文字・境界線が多く、ロッシー圧縮だと劣化が目立つためロスレスを使う。
const WEBP_EFFORT: f32 = 100.0;

/// 地図のズームレベル（大きいほど拡大）。地理院白地図(blank)は 5〜14 に対応。
const MAP_ZOOM: u8 = 8;

/// 出力地図のサイズ(px)。
const MAP_WIDTH: u32 = 640;
const MAP_HEIGHT: u32 = 480;

/// 複数マーカーに合わせた自動ズームの下限・上限と、縁の余白(px)。
/// 上限を抑えることで、1県のみのとき過度に拡大しすぎないようにする。
const FIT_ZOOM_MIN: u8 = 4;
const FIT_ZOOM_MAX: u8 = 8;
const FIT_PADDING: f64 = 64.0;

/// 震源付近へ寄せるかどうかを判断するズームのしきい値。
/// 全マーカーを収める枠だとこれ未満まで引きになる場合、弱い震度を枠の計算から外す。
const FOCUS_ZOOM_MIN: u8 = 7;

/// 枠の計算に含めるマーカーの震度スケール下限（緩い順に試す）。
/// 例: 30 なら震度3以上のマーカーと震源だけで枠を決める。
const FOCUS_SCALE_STEPS: [i32; 3] = [20, 30, 40];

/// 震源地を中心とした地図 WebP を生成し、バイト列で返す。
///
/// `scale` はマーカー色の決定に使う最大震度スケール。
pub fn render_quake_map(
    lat: f64,
    lon: f64,
    scale: i32,
    tile_url_template: &str,
) -> Result<Vec<u8>> {
    let mut map = StaticMapBuilder::new()
        .width(640)
        .height(480)
        .zoom(MAP_ZOOM)
        .url_template(tile_url_template)
        .build()?;

    let (r, g, b) = marker_rgb(scale);

    // 白い縁取り（視認性向上のため内側の円より少し大きく描く）。
    let outline = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(14.0)
        .build()?;

    // 震度に応じた色の中心円。
    let inner = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, r, g, b, 255))
        .radius(10.0)
        .build()?;

    map.add_tool(outline);
    map.add_tool(inner);

    // タイル取得とレンダリングを行い PNG バイト列を得る。
    // 注意: 内部のタイル取得は同期(ブロッキング)通信のため、
    // 呼び出し側で spawn_blocking 上から実行すること。
    let png = map.encode_png()?;

    // PNG を一旦デコードして WebP へ再エンコードし、ファイルサイズを軽量化する。
    encode_webp(&png)
}

/// 震源地と各地の震度をまとめてプロットした地図 WebP を生成し、バイト列で返す。
///
/// `markers` は `(緯度, 経度, 震度スケール)` の観測地点一覧。震源には黒縁の、
/// 各観測地点には白縁の震度色マーカーを描き、見た目で区別できるようにする。
/// 中心とズームは震源と主なマーカーが収まるよう自動計算する（`focus_bounds` 参照。
/// 震度1〜2が広範囲に及ぶ場合は震源付近へ寄せるため、遠方の弱いマーカーは枠外になりうる）。
/// `markers` が空の場合は `render_quake_map` と同じ震源のみの地図になる。
pub fn render_quake_map_with_points(
    lat: f64,
    lon: f64,
    scale: i32,
    markers: &[(f64, f64, i32)],
    tile_url_template: &str,
) -> Result<Vec<u8>> {
    if markers.is_empty() {
        return render_quake_map(lat, lon, scale, tile_url_template);
    }

    let view = focus_bounds(Some((lat, lon)), markers);
    let (lat_min, lat_max, lon_min, lon_max) = view;

    let zoom = fit_zoom(lat_min, lat_max, lon_min, lon_max);
    let mut map = StaticMapBuilder::new()
        .width(MAP_WIDTH)
        .height(MAP_HEIGHT)
        .zoom(zoom)
        .lat_center((lat_min + lat_max) / 2.0)
        .lon_center((lon_min + lon_max) / 2.0)
        .url_template(tile_url_template)
        .build()?;

    // 震度の弱い順に観測地点マーカーを描き、強い揺れが手前(上)に来るようにする。
    // 地点数が多い詳報では市区町村単位で密集するため、半径を絞って重なりを抑える。
    // 数えるのは枠内の地点だけ（枠外は画面外にクリップされ、重なりに関与しない）。
    let (outline_r, inner_r) = marker_radii(count_within(view, markers));
    let mut ordered: Vec<&(f64, f64, i32)> = markers.iter().collect();
    ordered.sort_by_key(|m| m.2);
    for &(m_lat, m_lon, m_scale) in ordered {
        let (outline, inner) = marker_pair(m_lat, m_lon, m_scale, outline_r, inner_r)?;
        map.add_tool(outline);
        map.add_tool(inner);
    }

    // 震源地は最後に描き、黒縁の二重円で観測地点マーカーと区別する。
    let (epi_outline, epi_inner) = epicenter_marker_pair(lat, lon, scale)?;
    map.add_tool(epi_outline);
    map.add_tool(epi_inner);

    let png = map.encode_png()?;
    encode_webp(&png)
}

/// 震源不明時のフォールバック地図。観測した都道府県ごとのマーカーを描いた WebP を返す。
///
/// `markers` は `(緯度, 経度, 震度スケール)` の一覧。主なマーカーが収まるよう中心とズームを
/// 自動計算する（`focus_bounds` 参照。ズームは過度な拡大を避けるため `FIT_ZOOM_MAX` で頭打ち）。
/// 注意: `render_quake_map` と同様、タイル取得がブロッキングのため spawn_blocking 上で呼ぶこと。
pub fn render_markers_map(markers: &[(f64, f64, i32)], tile_url_template: &str) -> Result<Vec<u8>> {
    if markers.is_empty() {
        return Err(anyhow!("描画するマーカーがありません"));
    }

    let view = focus_bounds(None, markers);
    let (lat_min, lat_max, lon_min, lon_max) = view;

    let zoom = fit_zoom(lat_min, lat_max, lon_min, lon_max);
    let mut map = StaticMapBuilder::new()
        .width(MAP_WIDTH)
        .height(MAP_HEIGHT)
        .zoom(zoom)
        .lat_center((lat_min + lat_max) / 2.0)
        .lon_center((lon_min + lon_max) / 2.0)
        .url_template(tile_url_template)
        .build()?;

    // 震度の弱い順に追加し、強い揺れのマーカーが手前(上)に来るようにする。
    // 地点数が多い詳報では市区町村単位で密集するため、半径を絞って重なりを抑える。
    // 数えるのは枠内の地点だけ（枠外は画面外にクリップされ、重なりに関与しない）。
    let (outline_r, inner_r) = marker_radii(count_within(view, markers));
    let mut ordered: Vec<&(f64, f64, i32)> = markers.iter().collect();
    ordered.sort_by_key(|m| m.2);
    for &(lat, lon, scale) in ordered {
        let (outline, inner) = marker_pair(lat, lon, scale, outline_r, inner_r)?;
        map.add_tool(outline);
        map.add_tool(inner);
    }

    let png = map.encode_png()?;
    encode_webp(&png)
}

/// 観測地点マーカーの半径(px)を `(縁, 中心)` で返す。`count` は枠内に入る地点数。
///
/// 詳報では市区町村単位で数百件の観測点が密集しうるため、地点数が多いほど
/// 半径を絞り、重なり合って判読しづらくなるのを防ぐ。とはいえ小さすぎると
/// 震度の色が読み取れないため、下限は潰れない程度に留める。
fn marker_radii(count: usize) -> (f32, f32) {
    match count {
        0..=15 => (12.0, 9.0),
        16..=60 => (9.0, 6.5),
        61..=200 => (7.0, 5.0),
        _ => (5.5, 4.0),
    }
}

/// 枠 `(lat_min, lat_max, lon_min, lon_max)` の内側にあるマーカー数を返す。
///
/// 枠外のマーカーは画面外へクリップされて重なりに関与しないため、
/// `marker_radii` の判断からは除く。
fn count_within(view: (f64, f64, f64, f64), markers: &[(f64, f64, i32)]) -> usize {
    let (lat_min, lat_max, lon_min, lon_max) = view;
    markers
        .iter()
        .filter(|&&(lat, lon, _)| {
            (lat_min..=lat_max).contains(&lat) && (lon_min..=lon_max).contains(&lon)
        })
        .count()
}

/// 震度色の二重円マーカー（白縁＋中心円）を作る。
fn marker_pair(
    lat: f64,
    lon: f64,
    scale: i32,
    outline_radius: f32,
    inner_radius: f32,
) -> Result<(Circle, Circle)> {
    let (r, g, b) = marker_rgb(scale);
    let outline = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(outline_radius)
        .build()?;
    let inner = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, r, g, b, 255))
        .radius(inner_radius)
        .build()?;
    Ok((outline, inner))
}

/// 震源色の二重円マーカー（黒縁＋中心円）を作る。観測地点マーカー（白縁）と区別するため使う。
fn epicenter_marker_pair(lat: f64, lon: f64, scale: i32) -> Result<(Circle, Circle)> {
    let (r, g, b) = marker_rgb(scale);
    let outline = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, 0, 0, 0, 255))
        .radius(14.0)
        .build()?;
    let inner = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, r, g, b, 255))
        .radius(10.0)
        .build()?;
    Ok((outline, inner))
}

/// 地図に収める範囲 `(lat_min, lat_max, lon_min, lon_max)` を決める。
///
/// 素直に全マーカーを収めると、震度1〜2だけが遠方まで散った地震では日本全体が写り、
/// 肝心の震源付近が潰れてしまう。そこで、全マーカーの枠のズームが `FOCUS_ZOOM_MIN`
/// 未満になる場合は、枠の計算に含めるマーカーを `FOCUS_SCALE_STEPS` の順に強い震度へ
/// 絞り込み、震源付近へ寄せる。強い揺れ自体が広域に及ぶ地震では絞っても引きのままだが、
/// その場合は広く写すのが正しいので問題にならない。
///
/// `anchor` は必ず枠に含める座標（震源。震源不明の 551 速報では `None`）。
/// 枠から外れたマーカーも描画自体は行われ、画面外へクリップされるだけ。
fn focus_bounds(anchor: Option<(f64, f64)>, markers: &[(f64, f64, i32)]) -> (f64, f64, f64, f64) {
    let bounds_of = |min_scale: i32| {
        let coords = anchor
            .into_iter()
            .chain(
                markers
                    .iter()
                    .filter(|m| m.2 >= min_scale)
                    .map(|&(lat, lon, _)| (lat, lon)),
            )
            .collect::<Vec<_>>();
        bounds(&coords)
    };

    // しきい値なしの枠。anchor か markers の少なくとも一方があれば必ず得られる。
    let mut best = bounds_of(i32::MIN).unwrap_or((0.0, 0.0, 0.0, 0.0));

    for min_scale in FOCUS_SCALE_STEPS {
        let (lat_min, lat_max, lon_min, lon_max) = best;
        if fit_zoom(lat_min, lat_max, lon_min, lon_max) >= FOCUS_ZOOM_MIN {
            break;
        }
        // 絞り込んだ結果マーカーが1つも残らないなら、これ以上は絞らない
        // （震源1点だけの枠になり、揺れの広がりが全く見えなくなるため）。
        if !markers.iter().any(|m| m.2 >= min_scale) {
            break;
        }
        if let Some(b) = bounds_of(min_scale) {
            best = b;
        }
    }

    best
}

/// 座標一覧のバウンディングボックス `(lat_min, lat_max, lon_min, lon_max)` を返す。空なら None。
fn bounds(coords: &[(f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    let (&(lat0, lon0), rest) = coords.split_first()?;
    Some(rest.iter().fold(
        (lat0, lat0, lon0, lon0),
        |(lat_min, lat_max, lon_min, lon_max), &(lat, lon)| {
            (
                lat_min.min(lat),
                lat_max.max(lat),
                lon_min.min(lon),
                lon_max.max(lon),
            )
        },
    ))
}

/// 緯度経度のバウンディングボックスが収まる最大ズームを返す（Webメルカトル基準）。
/// `FIT_ZOOM_MIN`〜`FIT_ZOOM_MAX` にクランプする。
fn fit_zoom(lat_min: f64, lat_max: f64, lon_min: f64, lon_max: f64) -> u8 {
    for z in (FIT_ZOOM_MIN..=FIT_ZOOM_MAX).rev() {
        let width_px = (lon_to_x(lon_max, z) - lon_to_x(lon_min, z)) * 256.0;
        // lat_to_y は緯度が下がるほど増えるため lat_min 側が大きい。
        let height_px = (lat_to_y(lat_min, z) - lat_to_y(lat_max, z)) * 256.0;
        if width_px <= f64::from(MAP_WIDTH) - FIT_PADDING
            && height_px <= f64::from(MAP_HEIGHT) - FIT_PADDING
        {
            return z;
        }
    }
    FIT_ZOOM_MIN
}

/// 経度をズーム z のタイル座標 (0..2^z) へ変換する。
fn lon_to_x(lon: f64, zoom: u8) -> f64 {
    ((lon + 180.0) / 360.0) * 2f64.powi(zoom.into())
}

/// 緯度をズーム z のタイル座標 (0..2^z) へ変換する（Webメルカトル）。
fn lat_to_y(lat: f64, zoom: u8) -> f64 {
    let rad = lat.to_radians();
    (1.0 - (rad.tan() + 1.0 / rad.cos()).ln() / PI) / 2.0 * 2f64.powi(zoom.into())
}

/// PNG バイト列をデコードし、WebP(ロスレス)へ再エンコードして返す。
fn encode_webp(png: &[u8]) -> Result<Vec<u8>> {
    let rgba = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8();
    let (w, h) = rgba.dimensions();

    let encoder = webp::Encoder::from_rgba(rgba.as_raw(), w, h);
    let webp = encoder
        .encode_simple(true, WEBP_EFFORT)
        .map_err(|e| anyhow!("WebP エンコードに失敗: {e:?}"))?;

    Ok(webp.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 大分県沖あたりを震源に見立てた座標。
    const EPICENTER: (f64, f64) = (33.2, 131.6);

    fn zoom_of(b: (f64, f64, f64, f64)) -> u8 {
        fit_zoom(b.0, b.1, b.2, b.3)
    }

    #[test]
    fn focus_drops_distant_weak_markers() {
        // 震源付近に強い揺れ、遠方（近畿・東北）に震度1〜2が散っているケース。
        // 全部を収めると日本全体まで引いてしまうので、弱い方から枠外にして寄せる。
        let markers = vec![
            (33.2, 131.6, 45),
            (33.5, 131.4, 40),
            (34.7, 135.5, 20), // 大阪付近・震度2
            (39.7, 140.1, 10), // 秋田付近・震度1
        ];
        let b = focus_bounds(Some(EPICENTER), &markers);

        assert!(b.1 < 39.0, "遠方の震度1が枠から外れる: {b:?}");
        assert!(b.3 < 140.0, "遠方の震度1が枠から外れる: {b:?}");
        // 震源は必ず枠内に残る。
        assert!(b.0 <= EPICENTER.0 && EPICENTER.0 <= b.1);
        assert!(b.2 <= EPICENTER.1 && EPICENTER.1 <= b.3);
        assert!(zoom_of(b) >= FOCUS_ZOOM_MIN, "震源付近まで寄る: {b:?}");
    }

    #[test]
    fn focus_keeps_bounds_when_already_close() {
        // 元から狭い範囲に収まっていれば絞り込みは働かない。
        let markers = vec![(33.2, 131.6, 45), (33.5, 131.4, 30)];
        assert_eq!(
            focus_bounds(Some(EPICENTER), &markers),
            (33.2, 33.5, 131.4, 131.6)
        );
    }

    #[test]
    fn focus_keeps_bounds_when_strong_shaking_is_wide() {
        // 強い揺れ自体が広域に及ぶ地震は、絞っても引きのまま＝広く写すのが正しい。
        let markers = vec![(33.2, 131.6, 50), (39.7, 140.1, 50)];
        assert_eq!(
            focus_bounds(Some(EPICENTER), &markers),
            (33.2, 39.7, 131.6, 140.1)
        );
    }

    #[test]
    fn focus_keeps_bounds_when_all_markers_are_weak() {
        // 全地点が震度1（深発地震の異常震域など）では、絞ると震源1点だけになり
        // 揺れの広がりが見えなくなるため、枠は広いまま維持する。
        let markers = vec![(33.2, 131.6, 10), (39.7, 140.1, 10)];
        assert_eq!(
            focus_bounds(Some(EPICENTER), &markers),
            (33.2, 39.7, 131.6, 140.1)
        );
    }

    #[test]
    fn focus_works_without_epicenter() {
        // 震源不明（震度速報）でも、強く揺れた地域へ寄せる。
        let markers = vec![(33.2, 131.6, 45), (33.5, 131.4, 40), (39.7, 140.1, 10)];
        let b = focus_bounds(None, &markers);
        assert!(b.1 < 39.0, "遠方の震度1が枠から外れる: {b:?}");
        assert!(
            zoom_of(b) >= FOCUS_ZOOM_MIN,
            "強い揺れの範囲まで寄る: {b:?}"
        );
    }

    #[test]
    fn radii_count_ignores_markers_outside_the_view() {
        // 枠外のマーカーは画面外にクリップされるので、半径の判断には数えない。
        let markers = vec![
            (33.2, 131.6, 45),
            (33.5, 131.4, 40),
            (39.7, 140.1, 10), // 枠外
        ];
        let view = (33.2, 33.5, 131.4, 131.6);
        assert_eq!(count_within(view, &markers), 2);
    }

    #[test]
    fn fit_zoom_is_clamped() {
        // ほぼ1点なら上限、日本全域なら下限。
        assert_eq!(fit_zoom(33.2, 33.2, 131.6, 131.6), FIT_ZOOM_MAX);
        assert_eq!(fit_zoom(24.0, 45.5, 123.0, 149.0), FIT_ZOOM_MIN);
    }
}
