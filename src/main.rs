//! 地震bot エントリポイント。
//!
//! P2P地震情報の WebSocket を購読し、日本国内の強い揺れの地震を
//! 地図画像付きで Discord Webhook へ通知する。

mod config;
mod discord;
mod geo;
mod intensity;
mod mapgen;
mod model;

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::intensity::{decide, decide_eew, eew_max_scale, eew_summary, tsunami_grade_rank};
use crate::model::{Eew, Envelope, JmaQuake, Tsunami};

/// 地震情報メッセージの code。
const CODE_JMA_QUAKE: i32 = 551;
/// 緊急地震速報（警報）メッセージの code。
const CODE_EEW: i32 = 556;
/// 津波予報メッセージの code。
const CODE_TSUNAMI: i32 = 552;

/// 無通信をどれだけ許容するか。これを超えたら接続が死んだとみなして張り直す。
///
/// サーバは実測でおよそ54秒ごとに ping を送ってくるため、正常な接続がこの時間だけ
/// 黙ることはない。逆にこの上限が無いと、ネットワークが黙って切れた（FIN も RST も
/// 届かない）場合に読み取りが永久に待ち続け、二度と通知が出ないまま生き残ってしまう。
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);

/// 接続（DNS・TCP・TLS・WebSocket ハンドシェイク）を諦めるまでの時間。
/// ここにも上限が無いと、経路が死んでいるときに接続処理のまま止まりうる。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 再接続バックオフの下限。正常な切断でも最低これだけは空け、連続切断時の暴走を防ぐ。
const BACKOFF_MIN: Duration = Duration::from_millis(500);
/// 再接続バックオフの上限。緊急地震速報は数十秒が勝負なので、長く待たない。
const BACKOFF_MAX: Duration = Duration::from_secs(8);
/// この時間つながっていられたら、次の切断ではバックオフを下限に戻す。
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// 接続が 429（レート制限）で拒否されたときの待ち時間。
///
/// このサーバは1つの IP から同時に1接続しか受け付けず、切れたセッションが解放される
/// までに十数秒かかる（実測）。解放前に叩き直しても弾かれ続けるだけなので、
/// 通常のバックオフとは別に、解放を待つ時間を空ける。
const BACKOFF_RATE_LIMITED: Duration = Duration::from_secs(10);

/// 切断中の取りこぼしを履歴 API で確認するときの制限時間。
/// 確認は再接続の前に行うため、長引かせると復帰そのものが遅れる。
const CATCH_UP_TIMEOUT: Duration = Duration::from_secs(8);

/// ワーカーの待ち行列の長さ。通常の受信頻度に対して十分な余裕を取る。
const WORKER_QUEUE_CAPACITY: usize = 256;

/// 地図生成（タイル取得を含む）を諦めるまでの時間。
///
/// タイル取得は同期通信でリクエスト全体のタイムアウトを持たないため、タイルサーバや
/// 経路が不調だと数十秒かかりうる。速報性を地図より優先し、間に合わなければ
/// テキストのみで通知する。
const MAP_TIMEOUT: Duration = Duration::from_secs(10);

/// 重複抑制のため記録する ID の上限。
const SEEN_ID_CAPACITY: usize = 256;

/// 通知済みの ID を保持し、重複・続報を抑制する汎用の記録。
#[derive(Default)]
struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenIds {
    /// 未登録なら記録して true を返す。既に登録済みなら false。
    fn mark_if_new(&mut self, id: &str) -> bool {
        if !self.set.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > SEEN_ID_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// 地震情報(551)の1報あたりの投稿状態。差し替え用に message_id と内容ハッシュを保持する。
struct QuakePost {
    /// 表示内容のハッシュ。一致すれば再投稿しない。
    signature: u64,
    /// 投稿済み Discord メッセージの ID。内容変更時はこれを編集する。
    message_id: String,
}

/// 同一地震（発生時刻キー）について、速報・詳報それぞれの投稿状態を別管理する。
#[derive(Default)]
struct QuakeEntry {
    /// 震度速報（ScalePrompt）の投稿状態。
    prompt: Option<QuakePost>,
    /// 詳報（各地の震度など）の投稿状態。
    detail: Option<QuakePost>,
}

/// 地震情報(551)の投稿状態を発生時刻ごとに保持する。容量超過で古い順に退避する。
#[derive(Default)]
struct QuakeTracker {
    map: HashMap<String, QuakeEntry>,
    order: VecDeque<String>,
}

impl QuakeTracker {
    /// 発生時刻キーのエントリを取得（無ければ作成）する。
    fn entry(&mut self, key: &str) -> &mut QuakeEntry {
        if !self.map.contains_key(key) {
            self.map.insert(key.to_string(), QuakeEntry::default());
            self.order.push_back(key.to_string());
            if self.order.len() > SEEN_ID_CAPACITY {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        self.map.get_mut(key).expect("直前に挿入済み")
    }
}

/// 緊急地震速報(556)の投稿状態を eventId ごとに保持する。
///
/// 続報で予想が変わったら同じメッセージを差し替えるため message_id を持つ。
/// 続報を捨てると、通知後に予想が引き下げられた場合に古い（過大な）内容が残り続ける。
#[derive(Default)]
struct EewTracker {
    map: HashMap<String, QuakePost>,
    order: VecDeque<String>,
}

impl EewTracker {
    /// 投稿済みか。取消報を通知してよいかの判定にも使う。
    fn contains(&self, event_id: &str) -> bool {
        self.map.contains_key(event_id)
    }

    /// 投稿済みなら `(message_id, signature)` を返す。未投稿は None。
    fn posted(&self, event_id: &str) -> Option<(String, u64)> {
        self.map
            .get(event_id)
            .map(|p| (p.message_id.clone(), p.signature))
    }

    /// 差し替え後の内容ハッシュを記録する。
    fn update_signature(&mut self, event_id: &str, signature: u64) {
        if let Some(post) = self.map.get_mut(event_id) {
            post.signature = signature;
        }
    }

    /// 新規投稿を記録する。容量超過で古い順に退避する。
    fn insert(&mut self, event_id: &str, post: QuakePost) {
        self.map.insert(event_id.to_string(), post);
        self.order.push_back(event_id.to_string());
        if self.order.len() > SEEN_ID_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// 表示用 payload から内容シグネチャ（ハッシュ）を求める。表示フィールドが全て反映される。
fn signature_of(payload: &serde_json::Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    payload.to_string().hash(&mut hasher);
    hasher.finish()
}

/// 種別ごとの重複抑制状態。
#[derive(Default)]
struct DedupState {
    /// 緊急地震速報の投稿状態（eventId ごと。続報は差し替え）。
    eews: EewTracker,
    /// 津波予報の id（同一発表の再送を除去）。
    tsunamis: SeenIds,
    /// 地震情報(551)の投稿状態（速報・詳報を発生時刻ごとに保持）。
    quakes: QuakeTracker,
}

#[tokio::main]
async fn main() -> Result<()> {
    // .env があれば読み込む（無ければ無視）。
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    info!(
        ws_url = %config.ws_url,
        regions = config.region_min_scales.len(),
        other_min = config.other_min_scale,
        attach_map = config.attach_map,
        "地震botを起動しました"
    );

    let http = reqwest::Client::builder()
        .user_agent("quake-alert-bot/0.1 (+https://github.com/)")
        .timeout(Duration::from_secs(30))
        .build()?;

    // テストモード: 過去のデータを1件取得して送信し終了する。
    if std::env::args().any(|a| a == "--test-tsunami") {
        return run_test_tsunami(&config, &http).await;
    }
    if std::env::args().any(|a| a == "--test-eew") {
        return run_test_eew(&config, &http).await;
    }
    if std::env::args().any(|a| a == "--test-prompt") {
        return run_test_prompt(&config, &http).await;
    }
    if std::env::args().any(|a| a == "--test") {
        return run_test(&config, &http).await;
    }

    // 受信と通知処理を分ける。受信ループは届いたテキストをワーカーへ渡すだけにし、
    // 地図生成や Discord 送信で止まらないようにする（理由は run_once を参照）。
    // 重複抑制の状態は種別ごとに独立しているため、緊急地震速報だけ別ワーカーにしても
    // 支障はない。地図生成の重い地震情報(551)に速報が待たされるのを避ける。
    let eew_tx = spawn_worker(config.clone(), http.clone(), "eew");
    let other_tx = spawn_worker(config.clone(), http.clone(), "quake");

    // 最後に処理した地震情報(551)の id。切断中に流れた分を履歴から拾い直す基準にする。
    let mut cursor: Option<String> = None;

    // 切断されても再接続し続ける。
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        // 接続する前に取りこぼしを拾う。接続後に取りに行くと、その間 ping に
        // 応答できず接続を失いかねない。
        catch_up(&http, &other_tx, &mut cursor).await;
        let result = run_once(&config, &eew_tx, &other_tx, &mut cursor).await;
        let rate_limited = match &result {
            Ok(()) => {
                warn!("WebSocket 接続が終了しました。再接続します");
                false
            }
            Err(e) => {
                error!(error = %e, "接続エラー。再接続します");
                is_rate_limited(e)
            }
        };

        // しばらく保っていた接続の切断は一時的なものとみなし、待ち時間を戻す。
        if started.elapsed() >= STABLE_CONNECTION {
            backoff = BACKOFF_MIN;
        }
        // 正常終了でも必ず間隔を空ける。空けないと、接続直後に切られ続ける状況で
        // 再接続を延々と繰り返してしまう。
        let wait = if rate_limited {
            BACKOFF_RATE_LIMITED
        } else {
            backoff
        };
        info!(wait_ms = wait.as_millis() as u64, rate_limited, "再接続まで待機します");
        tokio::time::sleep(wait).await;
        backoff = next_backoff(backoff);
    }
}

/// 次の再接続までの待ち時間。上限まで倍々に伸ばす。
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
}

/// 接続がレート制限（HTTP 429）で拒否されたか。
fn is_rate_limited(error: &anyhow::Error) -> bool {
    use tokio_tungstenite::tungstenite::{http::StatusCode, Error};
    matches!(
        error.downcast_ref::<Error>(),
        Some(Error::Http(resp)) if resp.status() == StatusCode::TOO_MANY_REQUESTS
    )
}

/// 受信テキストを1件ずつ処理するワーカーを起動し、その送信口を返す。
///
/// 通知処理（地図生成・Discord 送信）は受信ループではなくこちらで行う。
fn spawn_worker(config: Config, http: reqwest::Client, kind: &'static str) -> mpsc::Sender<String> {
    let (tx, mut rx) = mpsc::channel::<String>(WORKER_QUEUE_CAPACITY);
    tokio::spawn(async move {
        // 重複報・再送を抑制する状態。再接続をまたいで保持する。
        let mut dedup = DedupState::default();
        while let Some(text) = rx.recv().await {
            if let Err(e) = handle_text(&config, &http, &text, false, &mut dedup).await {
                error!(error = %e, kind, "メッセージ処理に失敗");
            }
        }
    });
    tx
}

/// 1回の WebSocket セッションを処理する。サーバからの切断で Ok を返す。
///
/// この関数の中では通知処理を行わず、受信したテキストをワーカーへ渡すだけにする。
/// サーバは ping への pong を返さない接続を数秒で切るが、pong は読み取りの延長で
/// 送られるため、ここで時間のかかる処理をすると pong が間に合わず切断される。
async fn run_once(
    config: &Config,
    eew_tx: &mpsc::Sender<String>,
    other_tx: &mpsc::Sender<String>,
    cursor: &mut Option<String>,
) -> Result<()> {
    let connect = tokio_tungstenite::connect_async(config.ws_url.as_str());
    let (mut ws_stream, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| anyhow!("接続が {} 秒以内に確立しませんでした", CONNECT_TIMEOUT.as_secs()))??;
    info!("WebSocket に接続しました");

    loop {
        // 無通信が続く接続は死んでいるとみなす。ネットワークが黙って切れると
        // FIN も RST も届かず、読み取りは永久に待ち続けてしまう。
        let next = tokio::time::timeout(IDLE_TIMEOUT, ws_stream.next())
            .await
            .map_err(|_| {
                anyhow!(
                    "{} 秒間受信がありません。接続が切れたとみなします",
                    IDLE_TIMEOUT.as_secs()
                )
            })?;
        let Some(message) = next else { break };
        match message? {
            Message::Text(text) => dispatch(&text, eew_tx, other_tx, cursor),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => {
                info!("サーバから切断通知を受信");
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// 受信テキストを code で振り分けてワーカーへ渡す。
///
/// 待ち行列が詰まっている場合は捨てて記録する。受信ループを止めると pong が遅れ、
/// 接続ごと失って以降の続報も落とすため、待たずに次の受信へ進むことを優先する。
fn dispatch(
    text: &str,
    eew_tx: &mpsc::Sender<String>,
    other_tx: &mpsc::Sender<String>,
    cursor: &mut Option<String>,
) {
    // 想定外のフォーマットは無視する。
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(code) = value.get("code").and_then(serde_json::Value::as_i64) else {
        return;
    };
    let code = code as i32;
    let tx = match code {
        CODE_EEW => eew_tx,
        CODE_JMA_QUAKE | CODE_TSUNAMI => other_tx,
        _ => return,
    };
    match tx.try_send(text.to_string()) {
        Ok(()) => {
            // 渡せた分だけ基準を進める。渡せなかったものは次の再接続で拾い直す。
            if code == CODE_JMA_QUAKE {
                if let Some(id) = message_id(&value) {
                    *cursor = Some(id.to_string());
                }
            }
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            error!(code, "処理が追いつかずメッセージを破棄しました");
        }
        // ワーカーが落ちている。このまま動き続けても二度と通知できないので、
        // 黙って生き残らずに終了し、プロセス管理（systemd 等）に再起動させる。
        Err(mpsc::error::TrySendError::Closed(_)) => {
            error!(code, "通知ワーカーが停止しています。プロセスを終了します");
            std::process::exit(1);
        }
    }
}

/// メッセージの id。WebSocket は `_id`、履歴 API は `id` と名前が違うが値は同じ。
fn message_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("_id")
        .or_else(|| value.get("id"))
        .and_then(serde_json::Value::as_str)
}

/// 履歴（新しい順）から `previous` より後に流れたものを、古い順に取り出す。
///
/// `previous` が見つからない場合は全件を返す（履歴の範囲を超えて切れていた場合）。
fn missed_since<'a>(items: &'a [serde_json::Value], previous: &str) -> Vec<&'a serde_json::Value> {
    let mut missed: Vec<&serde_json::Value> = items
        .iter()
        .take_while(|v| message_id(v) != Some(previous))
        .collect();
    missed.reverse();
    missed
}

/// 切断中に流れた地震情報(551)を履歴 API から拾い直してワーカーへ渡す。
///
/// 起動直後（`cursor` が None）は基準を記録するだけで何も通知しない。
/// そうしないと、起動のたびに過去の地震をまとめて投稿してしまう。
///
/// 緊急地震速報(556)は対象にしない。揺れる前に知らせるための情報で、
/// 揺れが終わった後に流すと誤解を招くため。
async fn catch_up(
    http: &reqwest::Client,
    tx: &mpsc::Sender<String>,
    cursor: &mut Option<String>,
) {
    let items = match tokio::time::timeout(CATCH_UP_TIMEOUT, fetch_history(http, HISTORY_URL)).await
    {
        Ok(Ok(items)) => items,
        Ok(Err(e)) => {
            warn!(error = %e, "取りこぼしの確認に失敗しました");
            return;
        }
        Err(_) => {
            warn!("取りこぼしの確認が時間内に終わりませんでした");
            return;
        }
    };

    // 履歴は新しい順に並んでいる。
    let Some(newest) = items.first().and_then(message_id).map(str::to_string) else {
        return;
    };
    let Some(previous) = cursor.clone() else {
        *cursor = Some(newest);
        info!("起動時の基準を記録しました（起動前の地震は通知しません）");
        return;
    };
    if previous == newest {
        return;
    }

    let missed = missed_since(&items, &previous);
    if missed.len() == items.len() {
        warn!(count = missed.len(), "取りこぼしが履歴の範囲を超えている可能性があります");
    }
    info!(count = missed.len(), "切断中に流れた地震情報を拾い直します");
    for item in missed {
        if tx.try_send(item.to_string()).is_err() {
            // 基準は進めない。次の再接続でもう一度拾い直す。
            warn!("拾い直した地震情報をワーカーへ渡せませんでした");
            return;
        }
    }
    *cursor = Some(newest);
}

/// 履歴 API から JSON 配列を取得する。
async fn fetch_history(http: &reqwest::Client, url: &str) -> Result<Vec<serde_json::Value>> {
    let body = http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(serde_json::from_str(&body)?)
}

/// 過去の地震情報 (P2P地震情報 REST API) のエンドポイント。
const HISTORY_URL: &str = "https://api.p2pquake.net/v2/history?codes=551&limit=50";
/// 過去の緊急地震速報 (556) のエンドポイント。
const HISTORY_EEW_URL: &str = "https://api.p2pquake.net/v2/history?codes=556&limit=50";
/// 過去の津波予報 (552) のエンドポイント。
const HISTORY_TSUNAMI_URL: &str = "https://api.p2pquake.net/v2/history?codes=552&limit=50";

/// テスト用: 過去の地震情報から通知条件を満たす最新の1件を選び、
/// 本番と同じ経路 (handle_text) で Discord へ送信して終了する。
async fn run_test(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の地震情報を取得します");
    let items = fetch_history(http, HISTORY_URL).await?;
    info!(count = items.len(), "履歴を取得しました");

    // 通知条件を満たす最新の地震を1件だけ送信する。
    for item in &items {
        let text = item.to_string();
        let quake: JmaQuake = match serde_json::from_str(&text) {
            Ok(q) => q,
            Err(_) => continue,
        };
        let decision = decide(
            &quake.earthquake,
            &quake.points,
            &config.region_min_scales,
            config.other_min_scale,
        );
        if decision.notify {
            info!(
                place = %quake.earthquake.hypocenter.name,
                max_scale = quake.earthquake.max_scale,
                time = %quake.earthquake.time,
                "テスト送信する地震を選択しました"
            );
            handle_text(config, http, &text, true, &mut DedupState::default()).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に通知条件を満たす地震がありませんでした。しきい値を下げて再試行してください");
    Ok(())
}

/// テスト用: 過去の地震情報から「震源未確定（震度速報など）」かつ観測県があり、
/// 通知条件を満たす最新の1件を選び、本番と同じ経路 (handle_quake) で送信して終了する。
///
/// 通常の `--test` は最新の通知対象（多くは震源確定済みの詳報）を選ぶため、震源未確定時に
/// 使う観測県マーカーマップの経路を確認できない。本コマンドはその経路を狙って検証する。
async fn run_test_prompt(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の地震情報から震源未確定の報を取得します");
    let items = fetch_history(http, HISTORY_URL).await?;
    info!(count = items.len(), "履歴を取得しました");

    for item in &items {
        let text = item.to_string();
        let quake: JmaQuake = match serde_json::from_str(&text) {
            Ok(q) => q,
            Err(_) => continue,
        };
        // 震源座標が有効な報は震源マップ経路（=通常の --test で確認できる）なので除外。
        if quake.earthquake.hypocenter.has_valid_coords() {
            continue;
        }
        // 観測県マーカーが1つも作れない報も対象外。
        if geo::points_to_markers(&quake.points).is_empty() {
            continue;
        }
        let decision = decide(
            &quake.earthquake,
            &quake.points,
            &config.region_min_scales,
            config.other_min_scale,
        );
        if decision.notify {
            info!(
                max_scale = quake.earthquake.max_scale,
                time = %quake.earthquake.time,
                reason = %decision.reason,
                "テスト送信する報（震源未確定）を選択しました"
            );
            handle_text(config, http, &text, true, &mut DedupState::default()).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("震源未確定で通知条件を満たす報が履歴にありませんでした。しきい値（OTHER_MIN_SCALE 等）を下げて再試行してください");
    Ok(())
}

/// テスト用: 過去の緊急地震速報から通知条件を満たす最新の1件を選び、
/// 本番と同じ経路 (handle_eew) で Discord へ送信して終了する。
async fn run_test_eew(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の緊急地震速報を取得します");
    let items = fetch_history(http, HISTORY_EEW_URL).await?;
    info!(count = items.len(), "履歴を取得しました");

    // テスト送信なので重複抑制は効かせない（毎回新しい状態を渡す）。
    let mut tracker = EewTracker::default();
    for item in &items {
        let text = item.to_string();
        let eew: Eew = match serde_json::from_str(&text) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if eew.cancelled {
            continue;
        }
        let decision = decide_eew(&eew.areas, &config.region_min_scales, config.other_min_scale);
        if decision.notify {
            info!(
                place = %eew.earthquake.hypocenter.name,
                event_id = %eew.issue.event_id,
                reason = %decision.reason,
                "テスト送信する緊急地震速報を選択しました"
            );
            handle_eew(config, http, &text, true, &mut tracker).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に通知条件を満たす緊急地震速報がありませんでした。しきい値を下げて再試行してください");
    Ok(())
}

/// テスト用: 過去の津波予報から有効な1件を選び、本番経路 (handle_tsunami) で送信して終了する。
async fn run_test_tsunami(config: &Config, http: &reqwest::Client) -> Result<()> {
    info!("テストモード: 過去の津波予報を取得します");
    let items = fetch_history(http, HISTORY_TSUNAMI_URL).await?;
    info!(count = items.len(), "履歴を取得しました");

    let mut seen = SeenIds::default();
    for item in &items {
        let text = item.to_string();
        let tsunami: Tsunami = match serde_json::from_str(&text) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let has_grade = tsunami
            .areas
            .iter()
            .any(|a| tsunami_grade_rank(&a.grade) > 0);
        if !tsunami.cancelled && has_grade {
            info!(areas = tsunami.areas.len(), "テスト送信する津波予報を選択しました");
            handle_tsunami(config, http, &text, true, &mut seen).await?;
            info!("テスト送信が完了しました");
            return Ok(());
        }
    }

    warn!("直近の履歴に津波予報がありませんでした（津波予報は稀に発表されます）");
    Ok(())
}

/// 受信した1メッセージ(JSON文字列)を code で振り分けて処理する。
///
/// `is_test` が true の場合、Discord 通知にテスト送信である旨を明示する。
async fn handle_text(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    dedup: &mut DedupState,
) -> Result<()> {
    // まず code だけ取り出して種別を判定する。
    let envelope: Envelope = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Ok(()), // 想定外のフォーマットは無視
    };

    match envelope.code {
        CODE_JMA_QUAKE => handle_quake(config, http, text, is_test, &mut dedup.quakes).await,
        CODE_EEW => handle_eew(config, http, text, is_test, &mut dedup.eews).await,
        CODE_TSUNAMI => handle_tsunami(config, http, text, is_test, &mut dedup.tsunamis).await,
        _ => Ok(()),
    }
}

/// 通知に添付する地図画像を生成する。生成できない場合は None（テキストのみで通知）。
///
/// `hypocenter` が Some なら震源＋観測地点マーカーの地図、None なら観測地点マーカーのみの
/// 地図にフォールバックする。マーカーも無ければ地図は作らない。
/// `scale` は震源マーカーの色に使う最大震度スケール。
///
/// staticmap のタイル取得は同期通信なので spawn_blocking 上で実行する。加えて、
/// リクエスト全体のタイムアウトを持たないため `MAP_TIMEOUT` で打ち切る。打ち切っても
/// 走っているタイル取得自体は止められないが、通知はそれを待たずに先へ進める。
async fn render_map(
    config: &Config,
    hypocenter: Option<(f64, f64)>,
    scale: i32,
    markers: Vec<(f64, f64, i32)>,
) -> Option<Vec<u8>> {
    if !config.attach_map {
        return None;
    }
    let tile_tpl = config.tile_url_template.clone();
    let task = match hypocenter {
        Some((lat, lon)) => tokio::task::spawn_blocking(move || {
            mapgen::render_quake_map_with_points(lat, lon, scale, &markers, &tile_tpl)
        }),
        None if markers.is_empty() => return None,
        None => tokio::task::spawn_blocking(move || {
            mapgen::render_markers_map(&markers, &tile_tpl)
        }),
    };

    match tokio::time::timeout(MAP_TIMEOUT, task).await {
        Ok(Ok(Ok(bytes))) => Some(bytes),
        Ok(Ok(Err(e))) => {
            warn!(error = %e, "地図画像の生成に失敗。テキストのみで通知します");
            None
        }
        Ok(Err(e)) => {
            warn!(error = %e, "地図生成タスクが異常終了。テキストのみで通知します");
            None
        }
        Err(_) => {
            warn!(
                timeout_secs = MAP_TIMEOUT.as_secs(),
                "地図生成が時間内に終わらないためテキストのみで通知します"
            );
            None
        }
    }
}

/// 地震情報(551) を処理し、通知条件を満たせば通知する。
///
/// 同一地震（発生時刻キー）について速報（震度速報）と詳報（各地の震度など）を別管理し、
/// 内容が前回と同じなら投稿しない。内容が変わった場合は既存メッセージを差し替える（編集）。
async fn handle_quake(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    tracker: &mut QuakeTracker,
) -> Result<()> {
    let quake: JmaQuake = serde_json::from_str(text)?;
    let eq = &quake.earthquake;

    let decision = decide(
        eq,
        &quake.points,
        &config.region_min_scales,
        config.other_min_scale,
    );

    if !decision.notify {
        info!(
            max_scale = eq.max_scale,
            place = %eq.hypocenter.name,
            "通知条件を満たさないためスキップ"
        );
        return Ok(());
    }

    let is_prompt = quake.issue.is_prompt();
    let kind = if is_prompt { "震度速報" } else { "地震情報" };
    info!(
        kind,
        max_scale = eq.max_scale,
        place = %eq.hypocenter.name,
        reason = %decision.reason,
        "通知対象の地震を検出"
    );

    // 地図画像。震源座標が有効なら震源マップ、未確定（速報など）なら
    // 観測した都道府県ごとのマーカーマップにフォールバックする。
    let hypocenter = eq
        .hypocenter
        .has_valid_coords()
        .then_some((eq.hypocenter.latitude, eq.hypocenter.longitude));
    let image = render_map(
        config,
        hypocenter,
        eq.max_scale,
        geo::points_to_markers(&quake.points),
    )
    .await;

    let payload = discord::build_payload(&quake, &decision.reason, image.is_some(), is_test);
    let signature = signature_of(&payload);
    let key = eq.time.clone();

    // 発生時刻が不明な場合は重複判定・差し替えができないため、そのまま新規投稿する。
    if key.is_empty() {
        discord::send(http, &config.webhook_url, &payload, image).await?;
        info!(kind, "Discord へ通知しました（発生時刻不明のため重複判定なし）");
        return Ok(());
    }

    // 速報・詳報それぞれのスロットを取り出す。
    let entry = tracker.entry(&key);
    let slot = if is_prompt {
        &mut entry.prompt
    } else {
        &mut entry.detail
    };

    match slot {
        // 内容に変更なし → 投稿しない。
        Some(post) if post.signature == signature => {
            info!(kind, "内容に変更がないため投稿をスキップ");
        }
        // 内容が変わった → 既存メッセージを差し替え（編集）。
        Some(post) => {
            discord::edit_message(http, &config.webhook_url, &post.message_id, &payload, image)
                .await?;
            post.signature = signature;
            info!(kind, message_id = %post.message_id, "内容が変わったため差し替えました");
        }
        // 初報 → 新規投稿して message_id を記録する。
        None => {
            let message_id =
                discord::post_message(http, &config.webhook_url, &payload, image).await?;
            info!(kind, message_id = %message_id, "Discord へ通知しました");
            *slot = Some(QuakePost {
                signature,
                message_id,
            });
        }
    }

    Ok(())
}

/// 緊急地震速報(556) を処理する。
///
/// 第1報で速報し、同一 eventId の続報は同じメッセージを差し替える（551 と同じ方針）。
/// 取消報は速報済みの場合のみ通知する。
async fn handle_eew(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    tracker: &mut EewTracker,
) -> Result<()> {
    let eew: Eew = serde_json::from_str(text)?;
    let event_id = eew.issue.event_id.clone();

    // 取消報: 既に速報済みの地震だけ取消を通知する。
    if eew.cancelled {
        if event_id.is_empty() || (!is_test && !tracker.contains(&event_id)) {
            return Ok(());
        }
        info!(event_id = %event_id, "緊急地震速報の取消を受信");
        let payload = discord::build_eew_payload(&eew, "", false, is_test);
        discord::send(http, &config.webhook_url, &payload, None).await?;
        info!("緊急地震速報の取消を通知しました");
        return Ok(());
    }

    // eventId が無い報は差し替え対象を特定できないため扱わない。
    if event_id.is_empty() {
        return Ok(());
    }

    let decision = decide_eew(&eew.areas, &config.region_min_scales, config.other_min_scale);
    let posted = tracker.contains(&event_id);

    // 未通知のまま基準を下回る報は無視する。通知済みなら、基準を下回った続報でも
    // 差し替える。捨ててしまうと、引き下げられた予想が反映されず古い内容が残る。
    if !decision.notify && !posted {
        return Ok(());
    }

    // 基準を下回った続報には理由文が無いので、しきい値と無関係な説明文を使う。
    let reason = if decision.notify {
        decision.reason.clone()
    } else {
        eew_summary(&eew.areas)
    };

    info!(
        event_id = %event_id,
        serial = %eew.issue.serial,
        place = %eew.earthquake.hypocenter.name,
        reason = %reason,
        below_threshold = !decision.notify,
        "緊急地震速報を検出"
    );

    // 地図画像。震源座標が有効なら震源＋対象地域マーカーの地図、未確定なら
    // 対象地域マーカーのみの地図にフォールバックする（551 と同じ方針）。
    let hypo = &eew.earthquake.hypocenter;
    let hypocenter = hypo
        .has_valid_coords()
        .then_some((hypo.latitude, hypo.longitude));
    let image = render_map(
        config,
        hypocenter,
        eew_max_scale(&eew.areas),
        geo::eew_areas_to_markers(&eew.areas),
    )
    .await;

    let payload = discord::build_eew_payload(&eew, &reason, image.is_some(), is_test);
    let signature = signature_of(&payload);

    // 投稿済みなら同じメッセージを差し替え、未投稿なら新規投稿する（551 と同じ方針）。
    match tracker.posted(&event_id) {
        // 内容に変更なし → 投稿しない。EEW は数秒間隔で続報が来るため、
        // これがレート制限に対する主な歯止めになる。
        Some((_, sig)) if sig == signature => {
            info!(event_id = %event_id, "内容に変更がないため投稿をスキップ");
        }
        // 内容が変わった → 既存メッセージを差し替え（編集）。
        Some((message_id, _)) => {
            discord::edit_message(http, &config.webhook_url, &message_id, &payload, image).await?;
            tracker.update_signature(&event_id, signature);
            info!(event_id = %event_id, message_id = %message_id, "続報で内容が変わったため差し替えました");
        }
        // 第1報 → 新規投稿して message_id を記録する。
        None => {
            let message_id =
                discord::post_message(http, &config.webhook_url, &payload, image).await?;
            info!(event_id = %event_id, message_id = %message_id, "緊急地震速報を通知しました");
            tracker.insert(&event_id, QuakePost {
                signature,
                message_id,
            });
        }
    }

    Ok(())
}

/// 津波予報(552) を処理する。
///
/// 同一発表(`id`)の再送は抑制する。発表・解除いずれも通知する。
async fn handle_tsunami(
    config: &Config,
    http: &reqwest::Client,
    text: &str,
    is_test: bool,
    seen: &mut SeenIds,
) -> Result<()> {
    let tsunami: Tsunami = serde_json::from_str(text)?;

    // 同一発表の再送を除去する（id で重複判定）。
    if !tsunami.id.is_empty() && !seen.mark_if_new(&tsunami.id) {
        return Ok(());
    }

    if tsunami.cancelled {
        info!("津波予報の解除を受信");
        let payload = discord::build_tsunami_payload(&tsunami, is_test);
        discord::send(http, &config.webhook_url, &payload, None).await?;
        info!("津波予報の解除を通知しました");
        return Ok(());
    }

    // 有効な予報（注意報以上）が含まれない場合は通知しない。
    let max_rank = tsunami
        .areas
        .iter()
        .map(|a| tsunami_grade_rank(&a.grade))
        .max()
        .unwrap_or(0);
    if max_rank == 0 {
        return Ok(());
    }

    info!(areas = tsunami.areas.len(), "津波予報を検出");
    let payload = discord::build_tsunami_payload(&tsunami, is_test);
    discord::send(http, &config.webhook_url, &payload, None).await?;
    info!("津波予報を通知しました");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Earthquake, Hypocenter, JmaQuake, QuakeIssue};

    fn quake(issue_type: &str, max_scale: i32) -> JmaQuake {
        JmaQuake {
            code: 551,
            issue: QuakeIssue {
                issue_type: issue_type.to_string(),
            },
            earthquake: Earthquake {
                time: "2026/06/28 05:21:00".to_string(),
                max_scale,
                hypocenter: Hypocenter::default(),
                ..Default::default()
            },
            points: vec![],
        }
    }

    fn payload(issue_type: &str, max_scale: i32, reason: &str) -> serde_json::Value {
        discord::build_payload(&quake(issue_type, max_scale), reason, false, false)
    }

    #[test]
    fn backoff_stays_short_enough_for_eew() {
        let mut wait = BACKOFF_MIN;
        for _ in 0..10 {
            wait = next_backoff(wait);
        }
        assert_eq!(wait, BACKOFF_MAX);
        // 緊急地震速報は数十秒で終わるため、再接続の待ちが長すぎてはいけない。
        assert!(BACKOFF_MAX <= Duration::from_secs(10));
    }

    #[test]
    fn rate_limited_handshake_is_detected() {
        use tokio_tungstenite::tungstenite::http::{Response, StatusCode};
        use tokio_tungstenite::tungstenite::Error;

        // 1 IP から同時に張れる接続は1本だけで、2本目の握手は 429 で拒否される。
        let too_many = Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .body(None)
            .unwrap();
        assert!(is_rate_limited(&anyhow::Error::new(Error::Http(too_many))));

        let bad_gateway = Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(None)
            .unwrap();
        assert!(!is_rate_limited(&anyhow::Error::new(Error::Http(bad_gateway))));
        assert!(!is_rate_limited(&anyhow!("接続が確立しませんでした")));
    }

    #[test]
    fn idle_timeout_exceeds_server_ping_interval() {
        // サーバの ping は実測でおよそ54秒間隔。正常な接続を誤って切らないよう、
        // しきい値はそれより十分長く取る。
        assert!(IDLE_TIMEOUT >= Duration::from_secs(70));
    }

    #[test]
    fn dispatch_routes_by_code() {
        let (eew_tx, mut eew_rx) = mpsc::channel(4);
        let (other_tx, mut other_rx) = mpsc::channel(4);
        let mut cursor = None;

        dispatch(r#"{"code":556}"#, &eew_tx, &other_tx, &mut cursor);
        dispatch(r#"{"code":551,"_id":"a1"}"#, &eew_tx, &other_tx, &mut cursor);
        dispatch(r#"{"code":552}"#, &eew_tx, &other_tx, &mut cursor);
        // 対象外の code と壊れた JSON は捨てる。
        dispatch(r#"{"code":555}"#, &eew_tx, &other_tx, &mut cursor);
        dispatch("not json", &eew_tx, &other_tx, &mut cursor);

        // 緊急地震速報は地震情報と別の待ち行列に入り、地図生成に待たされない。
        assert_eq!(eew_rx.try_recv().unwrap(), r#"{"code":556}"#);
        assert!(eew_rx.try_recv().is_err());
        assert_eq!(other_rx.try_recv().unwrap(), r#"{"code":551,"_id":"a1"}"#);
        assert_eq!(other_rx.try_recv().unwrap(), r#"{"code":552}"#);
        assert!(other_rx.try_recv().is_err());

        // 取りこぼしの基準は地震情報(551)だけで進める。
        assert_eq!(cursor.as_deref(), Some("a1"));
    }

    #[test]
    fn missed_since_returns_newer_items_oldest_first() {
        // 履歴は新しい順。WebSocket は `_id`、履歴 API は `id` で同じ値を返す。
        let items: Vec<serde_json::Value> = ["n3", "n2", "n1", "seen", "older"]
            .iter()
            .map(|id| serde_json::json!({ "id": id, "code": 551 }))
            .collect();

        let missed: Vec<&str> = missed_since(&items, "seen")
            .iter()
            .map(|v| message_id(v).unwrap())
            .collect();
        assert_eq!(missed, ["n1", "n2", "n3"]);

        // 最新まで処理済みなら拾い直すものは無い。
        assert!(missed_since(&items, "n3").is_empty());

        // 基準が履歴に無い（長く切れていた）場合は全件が対象になる。
        assert_eq!(missed_since(&items, "unknown").len(), items.len());
    }

    #[test]
    fn message_id_reads_both_key_names() {
        assert_eq!(message_id(&serde_json::json!({ "_id": "ws" })), Some("ws"));
        assert_eq!(message_id(&serde_json::json!({ "id": "rest" })), Some("rest"));
        assert_eq!(message_id(&serde_json::json!({})), None);
    }

    #[test]
    fn signature_same_for_identical_content() {
        let a = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        let b = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        assert_eq!(signature_of(&a), signature_of(&b));
    }

    #[test]
    fn signature_differs_when_scale_changes() {
        let a = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        let b = payload("ScalePrompt", 50, "東北で最大震度5強を観測");
        assert_ne!(signature_of(&a), signature_of(&b));
    }

    #[test]
    fn signature_differs_between_prompt_and_detail() {
        // 速報と詳報はタイトルが変わるためシグネチャも異なる（別管理の裏付け）。
        let prompt = payload("ScalePrompt", 45, "東北で最大震度5弱を観測");
        let detail = payload("DetailScale", 45, "東北で最大震度5弱を観測");
        assert_ne!(signature_of(&prompt), signature_of(&detail));
    }

    #[test]
    fn tracker_keeps_prompt_and_detail_separately() {
        let mut tracker = QuakeTracker::default();
        let key = "2026/06/28 05:21:00";
        {
            let entry = tracker.entry(key);
            entry.prompt = Some(QuakePost {
                signature: 1,
                message_id: "p".to_string(),
            });
        }
        let entry = tracker.entry(key);
        assert!(entry.prompt.is_some());
        assert!(entry.detail.is_none());
    }

    #[test]
    fn tracker_evicts_oldest_over_capacity() {
        let mut tracker = QuakeTracker::default();
        for i in 0..(SEEN_ID_CAPACITY + 5) {
            tracker.entry(&format!("key-{i}"));
        }
        assert_eq!(tracker.map.len(), SEEN_ID_CAPACITY);
        // 最初に入れたキーは退避されている。
        assert!(!tracker.map.contains_key("key-0"));
    }

    #[test]
    fn eew_tracker_keeps_message_id_for_replacement() {
        // 続報で差し替えられるよう、eventId から message_id を引けること。
        let mut tracker = EewTracker::default();
        assert!(tracker.posted("ev1").is_none());
        tracker.insert("ev1", QuakePost {
            signature: 1,
            message_id: "m1".to_string(),
        });
        assert!(tracker.contains("ev1"));
        assert_eq!(tracker.posted("ev1"), Some(("m1".to_string(), 1)));

        // 差し替え後はシグネチャだけ更新し、message_id は保つ。
        tracker.update_signature("ev1", 2);
        assert_eq!(tracker.posted("ev1"), Some(("m1".to_string(), 2)));
    }

    #[test]
    fn eew_tracker_evicts_oldest_over_capacity() {
        let mut tracker = EewTracker::default();
        for i in 0..(SEEN_ID_CAPACITY + 5) {
            tracker.insert(&format!("ev-{i}"), QuakePost {
                signature: 0,
                message_id: String::new(),
            });
        }
        assert_eq!(tracker.map.len(), SEEN_ID_CAPACITY);
        assert!(!tracker.contains("ev-0"));
    }

    fn eew_payload(areas: Vec<crate::model::EewArea>, reason: &str) -> serde_json::Value {
        let eew = Eew {
            code: 556,
            cancelled: false,
            issue: crate::model::EewIssue {
                event_id: "ev1".to_string(),
                serial: "1".to_string(),
                time: "2026/07/28 17:08:41".to_string(),
            },
            earthquake: Default::default(),
            areas,
        };
        discord::build_eew_payload(&eew, reason, false, false)
    }

    fn eew_area(pref: &str, name: &str, scale_from: i32, scale_to: i32) -> crate::model::EewArea {
        crate::model::EewArea {
            pref: pref.to_string(),
            name: name.to_string(),
            scale_from,
            scale_to,
        }
    }

    #[test]
    fn eew_signature_changes_when_forecast_is_revised() {
        // 第1報「5弱程度以上」→ 第2報「5強」で内容が変わるため差し替えが走ること。
        // シグネチャが同じだと引き下げが反映されず、古い内容が残ってしまう。
        let first = eew_payload(
            vec![eew_area("熊本", "熊本県熊本", 45, 99)],
            "熊本県で予想最大震度5弱程度以上",
        );
        let second = eew_payload(
            vec![eew_area("熊本", "熊本県熊本", 50, 50)],
            "熊本県で予想最大震度5強",
        );
        assert_ne!(signature_of(&first), signature_of(&second));

        // 同じ内容の再送では差し替えない（EEWは数秒間隔で続報が来るため）。
        let resend = eew_payload(
            vec![eew_area("熊本", "熊本県熊本", 50, 50)],
            "熊本県で予想最大震度5強",
        );
        assert_eq!(signature_of(&second), signature_of(&resend));
    }

    #[test]
    fn eew_summary_describes_forecast_without_threshold() {
        // 基準を下回った続報の差し替え本文。しきい値と無関係に全地域から求める。
        let areas = vec![
            eew_area("熊本", "熊本県熊本", 50, 50),
            eew_area("熊本", "熊本県球磨", 40, 40),
        ];
        assert_eq!(eew_summary(&areas), "熊本県で予想最大震度5強");
    }
}
