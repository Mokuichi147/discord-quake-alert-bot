//! 震源地をプロットした地図画像(WebP)の生成。
//!
//! staticmap クレートで地図タイル(既定は国土地理院の白地図)を取得し、震源地に二重円マーカーを描く。
//! staticmap は PNG しか出力できないため、生成後に WebP へ再エンコードして軽量化する。

use anyhow::{anyhow, Result};
use staticmap::{
    tools::{CircleBuilder, Color},
    StaticMapBuilder,
};

use crate::intensity::marker_rgb;

/// WebP ロスレス圧縮の努力度（0.0〜100.0、高いほど高圧縮）。
/// 地図は文字・境界線が多く、ロッシー圧縮だと劣化が目立つためロスレスを使う。
const WEBP_EFFORT: f32 = 100.0;

/// 地図のズームレベル（大きいほど拡大）。地理院白地図(blank)は 5〜14 に対応。
const MAP_ZOOM: u8 = 8;

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
