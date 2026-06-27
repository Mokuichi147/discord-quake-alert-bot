//! P2P地震情報 API (https://www.p2pquake.net/develop/json_api_v2/) のレスポンス型。
//!
//! WebSocket からは各種 `code` のメッセージが流れてくる。本botでは
//! `code == 551`（地震情報 = JMAQuake）のみを扱う。

use serde::Deserialize;

/// WebSocket で受信する各メッセージの共通ヘッダ。
/// まず `code` を見てメッセージ種別を判定する。
#[derive(Debug, Deserialize)]
pub struct Envelope {
    pub code: i32,
}

/// code == 551 の地震情報メッセージ全体。
#[derive(Debug, Deserialize)]
pub struct JmaQuake {
    pub code: i32,
    #[serde(default)]
    pub earthquake: Earthquake,
    #[serde(default)]
    pub points: Vec<Point>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Earthquake {
    /// 発生時刻（例: "2026/06/26 12:34:00"）。
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub hypocenter: Hypocenter,
    /// 最大震度。10=1, 20=2, 30=3, 40=4, 45=5弱, 50=5強, 55=6弱, 60=6強, 70=7, -1=不明。
    #[serde(rename = "maxScale", default = "minus_one")]
    pub max_scale: i32,
    /// 国内津波の有無（"None" / "Unknown" / "Checking" / "NonEffective" / "Watch" / "Warning"）。
    #[serde(rename = "domesticTsunami", default)]
    pub domestic_tsunami: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct Hypocenter {
    /// 震源地名（例: "千葉県北西部"）。不明時は空。
    #[serde(default)]
    pub name: String,
    /// 緯度。不明時は -200 などの無効値が入る。
    #[serde(default = "invalid_coord")]
    pub latitude: f64,
    /// 経度。不明時は -200 などの無効値が入る。
    #[serde(default = "invalid_coord")]
    pub longitude: f64,
    /// 深さ(km)。不明時は -1。
    #[serde(default = "minus_one_f")]
    pub depth: f64,
    /// マグニチュード。不明時は -1。
    #[serde(default = "minus_one_f")]
    pub magnitude: f64,
}

#[derive(Debug, Deserialize)]
pub struct Point {
    /// 都道府県名（例: "東京都"）。
    #[serde(default)]
    pub pref: String,
    /// 観測点または地域名。
    #[serde(default)]
    pub addr: String,
    /// その地点の震度スケール（Earthquake.max_scale と同じ値域）。
    #[serde(default = "minus_one")]
    pub scale: i32,
}

/// code == 556 の緊急地震速報（警報）メッセージ全体。
#[derive(Debug, Deserialize)]
pub struct Eew {
    pub code: i32,
    /// 取消報なら true。
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub issue: EewIssue,
    #[serde(default)]
    pub earthquake: EewEarthquake,
    /// 警報対象地域ごとの予想震度。取消報では空のことがある。
    #[serde(default)]
    pub areas: Vec<EewArea>,
}

#[derive(Debug, Default, Deserialize)]
pub struct EewIssue {
    /// 同一地震を束ねるID。重複報の判定に使う。
    #[serde(rename = "eventId", default)]
    pub event_id: String,
    /// 報番号（"1" が第1報）。
    #[serde(default)]
    pub serial: String,
    /// 発表時刻。
    #[serde(default)]
    pub time: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct EewEarthquake {
    /// 地震発生時刻。
    #[serde(rename = "originTime", default)]
    pub origin_time: String,
    #[serde(default)]
    pub hypocenter: Hypocenter,
}

#[derive(Debug, Deserialize)]
pub struct EewArea {
    /// 都府県名（例: "神奈川"。551と異なり接尾辞なし）。
    #[serde(default)]
    pub pref: String,
    /// 地域名（例: "神奈川県西部"）。
    #[serde(default)]
    pub name: String,
    /// 予想震度の下限スケール（551 と同じ値域）。
    #[serde(rename = "scaleFrom", default = "minus_one")]
    pub scale_from: i32,
    /// 予想震度の上限スケール（551 と同じ値域）。
    #[serde(rename = "scaleTo", default = "minus_one")]
    pub scale_to: i32,
}

fn minus_one() -> i32 {
    -1
}
fn minus_one_f() -> f64 {
    -1.0
}
fn invalid_coord() -> f64 {
    -200.0
}

impl Hypocenter {
    /// 地図に描画できる有効な座標を持っているか。
    pub fn has_valid_coords(&self) -> bool {
        (-90.0..=90.0).contains(&self.latitude)
            && (-180.0..=180.0).contains(&self.longitude)
            && !(self.latitude == 0.0 && self.longitude == 0.0)
    }
}
