//! 震源地をプロットした地図画像(WebP)の生成。
//!
//! staticmap クレートで地図タイル(既定は国土地理院の白地図)を取得し、震源地に二重円マーカーを描く。
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

/// 震源不明時のフォールバック地図。観測した都道府県ごとのマーカーを描いた WebP を返す。
///
/// `markers` は `(緯度, 経度, 震度スケール)` の一覧。全マーカーが収まるよう中心とズームを
/// 自動計算する（ズームは過度な拡大を避けるため `FIT_ZOOM_MAX` で頭打ちにする）。
/// 注意: `render_quake_map` と同様、タイル取得がブロッキングのため spawn_blocking 上で呼ぶこと。
pub fn render_markers_map(markers: &[(f64, f64, i32)], tile_url_template: &str) -> Result<Vec<u8>> {
    if markers.is_empty() {
        return Err(anyhow!("描画するマーカーがありません"));
    }

    let lat_min = markers.iter().map(|m| m.0).fold(f64::INFINITY, f64::min);
    let lat_max = markers.iter().map(|m| m.0).fold(f64::NEG_INFINITY, f64::max);
    let lon_min = markers.iter().map(|m| m.1).fold(f64::INFINITY, f64::min);
    let lon_max = markers.iter().map(|m| m.1).fold(f64::NEG_INFINITY, f64::max);

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
    let mut ordered: Vec<&(f64, f64, i32)> = markers.iter().collect();
    ordered.sort_by_key(|m| m.2);
    for &(lat, lon, scale) in ordered {
        let (outline, inner) = marker_pair(lat, lon, scale)?;
        map.add_tool(outline);
        map.add_tool(inner);
    }

    let png = map.encode_png()?;
    encode_webp(&png)
}

/// 震度色の二重円マーカー（白縁＋中心円）を作る。
fn marker_pair(lat: f64, lon: f64, scale: i32) -> Result<(Circle, Circle)> {
    let (r, g, b) = marker_rgb(scale);
    let outline = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, 255, 255, 255, 255))
        .radius(11.0)
        .build()?;
    let inner = CircleBuilder::new()
        .lon_coordinate(lon)
        .lat_coordinate(lat)
        .color(Color::new(true, r, g, b, 255))
        .radius(8.0)
        .build()?;
    Ok((outline, inner))
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
