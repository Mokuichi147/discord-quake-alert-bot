//! 震源地をプロットした地図画像(PNG)の生成。
//!
//! staticmap クレートで OpenStreetMap タイルを取得し、震源地に二重円マーカーを描く。

use anyhow::Result;
use staticmap::{
    tools::{CircleBuilder, Color},
    StaticMapBuilder,
};

use crate::intensity::marker_rgb;

/// 震源地を中心とした地図 PNG を生成し、バイト列で返す。
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
        .zoom(7)
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

    // タイル取得とレンダリングを行い PNG バイト列を返す。
    // 注意: 内部のタイル取得は同期(ブロッキング)通信のため、
    // 呼び出し側で spawn_blocking 上から実行すること。
    let bytes = map.encode_png()?;
    Ok(bytes)
}
