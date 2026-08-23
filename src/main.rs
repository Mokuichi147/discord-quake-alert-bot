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
use std::sync::{Arc, Mutex};
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
/// 公式仕様では `/ws` は IP アドレスごとに2接続までだが、実測では2本目の握手が
/// すぐ 429 になり、切れたセッションが解放されるまでにも十数秒かかった。解放前に
/// 叩き直しても弾かれ続けるだけなので、通常のバックオフとは別に待つ時間を空ける。
const BACKOFF_RATE_LIMITED: Duration = Duration::from_secs(10);

/// 切断中の取りこぼしを履歴 API で確認するときの、1種別あたりの制限時間。
///
/// 確認の間は受信を始めないため、サーバの ping（接続から約54秒後）に pong を
/// 返せなくなる前に必ず終わらせる。対象は2種別なので最悪でもこの2倍。
const CATCH_UP_TIMEOUT: Duration = Duration::from_secs(5);

/// 通知できなかった報を拾い直しに行く間隔。
///
/// 接続が続いている限り再接続は起きないため、これが無いと失敗した通知が
/// 次の切断まで（＝何時間も）再送されない。失敗が残っているときだけ動く。
const FAILED_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// 地震情報・津波予報の待ち行列の長さ。
///
/// 溢れた分は失敗として記録して捨て、履歴から拾い直す。取り戻せる種別なので、
/// 受信ループを止めてまで押し込まない。
const WORKER_QUEUE_CAPACITY: usize = 256;

/// 緊急地震速報の待ち行列に載せる地震の数の上限。
/// 同時に進行している地震の数だけあれば足りる（通常は1〜2件）。
const EEW_QUEUE_CAPACITY: usize = 32;

/// 緊急地震速報を通知する意味がある時間。
///
/// 待たされて古くなった報は投稿しない。揺れが終わった後に「これから強い揺れが
/// 来る」と伝えることになるため。
const EEW_MAX_AGE: Duration = Duration::from_secs(180);

/// 待ち行列を空にするのを待つ上限。
///
/// 待つ間は受信を止めるため、長くしすぎると pong を返せなくなる。これでも
/// 空かないほどワーカーが詰まっている場合は、その回の拾い直しを見送る。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// 記録済みか。
    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    /// 記録する。容量超過で古い順に退避する。
    ///
    /// 記録するのは通知できた後にする。送信前に記録すると、失敗した発表が
    /// 再送や履歴からの拾い直しで重複扱いになり、通知が永久に失われる。
    fn mark(&mut self, id: &str) {
        if !self.set.insert(id.to_string()) {
            return;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > SEEN_ID_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
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
    // 通知まで終えた最新のメッセージ id。切断中に流れた分を履歴から拾い直す基準にする。
    let cursors = Cursors::default();
    let eew_queue = EewQueue::default();
    spawn_eew_worker(config.clone(), http.clone(), eew_queue.clone());
    let other_tx = spawn_worker(config.clone(), http.clone(), cursors.clone());

    // 切断されても再接続し続ける。
    let mut backoff = BACKOFF_MIN;
    loop {
        let started = Instant::now();
        let result = run_once(&config, &http, &eew_queue, &other_tx, &cursors).await;
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
fn spawn_worker(config: Config, http: reqwest::Client, cursors: Cursors) -> mpsc::Sender<Job> {
    let (tx, mut rx) = mpsc::channel::<Job>(WORKER_QUEUE_CAPACITY);
    tokio::spawn(async move {
        // 重複報・再送を抑制する状態。再接続をまたいで保持する。
        let mut dedup = DedupState::default();
        while let Some(job) = rx.recv().await {
            let msg = match job {
                Job::Message(msg) => msg,
                Job::Barrier(done) => {
                    // 送り主はここまでの処理が終わるのを待っている。
                    let _ = done.send(());
                    continue;
                }
            };
            match handle_text(&config, &http, &msg.text, false, &mut dedup).await {
                // 通知まで終えたものだけ基準を進める。待ち行列に入れた時点で進めると、
                // Discord 送信に失敗した報が拾い直しの対象から外れて永久に失われる。
                Ok(()) => cursors.record_handled(&msg),
                Err(e) => {
                    // 失敗を覚えておき、この報を通知できるまで基準を進めない。
                    // 拾い直しで再送される。
                    cursors.record_failed(&msg);
                    error!(error = %e, code = msg.code, "メッセージ処理に失敗");
                }
            }
        }
    });
    tx
}

/// 緊急地震速報を1件ずつ処理するワーカーを起動する。
///
/// 地震情報(551)とは別のワーカーにして、地図生成の重い 551 に速報が待たされない
/// ようにする。重複抑制の状態は種別ごとに独立しているため分けても支障はない。
fn spawn_eew_worker(config: Config, http: reqwest::Client, queue: EewQueue) {
    tokio::spawn(async move {
        // 続報の差し替えに使う投稿状態。再接続をまたいで保持する。
        let mut tracker = EewTracker::default();
        loop {
            let pending = queue.pop().await;
            // 待っている間に古くなった報は投稿しない。
            let age = pending.received.elapsed();
            if age > EEW_MAX_AGE {
                warn!(
                    event_id = %pending.event_id,
                    age_secs = age.as_secs(),
                    "古くなった緊急地震速報を捨てました"
                );
                continue;
            }
            let result =
                handle_eew(&config, &http, &pending.message.text, false, &mut tracker).await;
            if let Err(e) = result {
                error!(error = %e, event_id = %pending.event_id, "緊急地震速報の処理に失敗");
            }
        }
    });
}

/// ワーカーへ1件渡す。
///
/// 待ち行列が満杯なら捨てて、失敗として記録する。記録しておけば履歴から拾い直せる
/// ので、受信ループを止めてまで押し込まない（止めると pong が遅れて接続を失う）。
fn enqueue(tx: &mpsc::Sender<Job>, cursors: &Cursors, message: Incoming) {
    let code = message.code;
    match tx.try_send(Job::Message(message)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(job)) => {
            if let Job::Message(message) = &job {
                cursors.record_failed(message);
            }
            error!(code, "待ち行列が満杯のため捨てました。履歴から拾い直します");
        }
        // ワーカーが落ちている。このまま動き続けても二度と通知できないので、
        // 黙って生き残らずに終了し、プロセス管理（systemd 等）に再起動させる。
        Err(mpsc::error::TrySendError::Closed(_)) => {
            error!(code, "通知ワーカーが停止しています。プロセスを終了します");
            std::process::exit(1);
        }
    }
}

/// 待ち行列に残っている分を処理し終えるまで待つ。空にできたら true。
///
/// 履歴からの拾い直しの前に呼び、古い履歴の報が新しいライブの報より後ろに
/// 並ばないようにする。
async fn drain(tx: &mpsc::Sender<Job>) -> bool {
    let (done, wait) = tokio::sync::oneshot::channel();
    // 満杯なら諦める。押し込もうと待つと受信ループが止まる。
    if tx.try_send(Job::Barrier(done)).is_err() {
        return false;
    }
    match tokio::time::timeout(DRAIN_TIMEOUT, wait).await {
        Ok(Ok(())) => true,
        _ => {
            warn!(
                timeout_secs = DRAIN_TIMEOUT.as_secs(),
                "待ち行列が空くのを待てませんでした"
            );
            false
        }
    }
}

/// 1回の WebSocket セッションを処理する。サーバからの切断で Ok を返す。
///
/// この関数の中では通知処理を行わず、受信したテキストをワーカーへ渡すだけにする。
/// サーバは ping への pong を返さない接続を数秒で切るが、pong は読み取りの延長で
/// 送られるため、ここで時間のかかる処理をすると pong が間に合わず切断される。
async fn run_once(
    config: &Config,
    http: &reqwest::Client,
    eew_queue: &EewQueue,
    other_tx: &mpsc::Sender<Job>,
    cursors: &Cursors,
) -> Result<()> {
    let connect = tokio_tungstenite::connect_async(config.ws_url.as_str());
    let (mut ws_stream, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| anyhow!("接続が {} 秒以内に確立しませんでした", CONNECT_TIMEOUT.as_secs()))??;
    info!("WebSocket に接続しました");

    // 購読を始めてから、受信を始める前に取りこぼしを拾う。
    //
    // 接続前に取りに行くと、履歴のレスポンスから購読開始までに流れた分がどちらにも
    // 含まれず抜ける。逆に受信と並行して行うと、ライブで受けた新しい報の後ろに
    // 古い履歴の報が並び、内容を古いものへ差し戻してしまう。
    //
    // ここで待っても、接続済みなのでその間の分は取りこぼさない（読み取るまで
    // バッファに残る）。最初の ping は接続から約54秒後なので、数秒の確認で
    // pong が遅れることもない。
    catch_up_all(http, other_tx, cursors).await;

    // 無通信が続く接続は死んでいるとみなす。ネットワークが黙って切れると
    // FIN も RST も届かず、読み取りは永久に待ち続けてしまう。
    // 受信したときだけ期限を延ばす（拾い直しでは延ばさない）。
    let idle = tokio::time::sleep(IDLE_TIMEOUT);
    tokio::pin!(idle);

    // 通知できなかった報を拾い直しに行く時計。接続が続く限り再接続は起きないため、
    // これが無いと失敗した通知が次の切断まで再送されない。
    let mut retry = tokio::time::interval(FAILED_RETRY_INTERVAL);
    retry.tick().await; // 最初の tick は即座に返るので捨てる

    loop {
        let message = tokio::select! {
            _ = &mut idle => {
                anyhow::bail!(
                    "{} 秒間受信がありません。接続が切れたとみなします",
                    IDLE_TIMEOUT.as_secs()
                );
            }
            _ = retry.tick() => {
                // 必要なときだけ取りに行く。受信を止めるのは、ライブの報と
                // 履歴の報が混ざらないようにするため。
                if cursors.needs_catch_up() {
                    info!("通知できていない報または未取得の基準があるため履歴を確認します");
                    catch_up_all(http, other_tx, cursors).await;
                }
                continue;
            }
            next = ws_stream.next() => {
                let Some(message) = next else { break };
                idle.as_mut().reset(tokio::time::Instant::now() + IDLE_TIMEOUT);
                message?
            }
        };
        match message {
            Message::Text(text) => dispatch(&text, eew_queue, other_tx, cursors),
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

/// ワーカーへの指示。
enum Job {
    /// 通知処理するメッセージ。
    Message(Incoming),
    /// ここまで処理し終えたら知らせる目印。
    ///
    /// 履歴からの拾い直しの前に流し、待ち行列に残っているライブの報を先に
    /// 処理させる。残したまま古い履歴の報を積むと、新しい内容が古い内容へ
    /// 差し戻されてしまう。
    Barrier(tokio::sync::oneshot::Sender<()>),
}

/// 待ち行列に載っている緊急地震速報1件。
struct PendingEew {
    /// 同一地震を束ねる ID。同じ地震の報が来たら差し替える。
    event_id: String,
    /// 受信した時刻。古くなった報を捨てる判断に使う。
    received: Instant,
    message: Incoming,
}

/// 緊急地震速報の待ち行列。
///
/// 続報は前の報を差し替えるため、eventId ごとに最新の1件だけを持つ。こうすると
/// 長さは同時進行している地震の数に収まり、処理が遅れていても常に最新の予想を
/// 投稿できる（遅れて古い続報を投稿し直すことがない）。
///
/// 緊急地震速報は履歴から拾い直さない方針なので、上限で捨てる作りにはできない。
/// 差し替え方式なら捨てずに済み、それでも際限なく積み上がることはない。
#[derive(Clone, Default)]
struct EewQueue {
    inner: Arc<Mutex<VecDeque<PendingEew>>>,
    ready: Arc<tokio::sync::Notify>,
}

impl EewQueue {
    /// 受信した報を積む。同じ地震の報が既にあれば差し替える。
    fn push(&self, event_id: String, message: Incoming) {
        let pending = PendingEew {
            event_id,
            received: Instant::now(),
            message,
        };
        {
            let mut queue = self.lock();
            match queue.iter_mut().find(|p| p.event_id == pending.event_id) {
                Some(slot) => *slot = pending,
                None => {
                    queue.push_back(pending);
                    // ここまで溜まるのは異常だが、際限なく積み上がらないよう
                    // 上限を設ける。捨てるのは最も古い＝最も価値の低い報。
                    if queue.len() > EEW_QUEUE_CAPACITY {
                        if let Some(dropped) = queue.pop_front() {
                            error!(
                                event_id = %dropped.event_id,
                                "待ち行列が溢れたため古い緊急地震速報を捨てました"
                            );
                        }
                    }
                }
            }
        }
        self.ready.notify_one();
    }

    /// 次の報を取り出す。無ければ届くまで待つ。
    async fn pop(&self) -> PendingEew {
        loop {
            // 待ち受けを先に用意する。確認してから用意すると、その間に入った分の
            // 通知を取りこぼす。
            let ready = self.ready.notified();
            if let Some(pending) = self.lock().pop_front() {
                return pending;
            }
            ready.await;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<PendingEew>> {
        self.inner.lock().expect("毒された Mutex")
    }
}

/// ワーカーへ渡す1件。振り分けの時点で取り出した code と id を添える。
struct Incoming {
    /// メッセージ種別。
    code: i32,
    /// メッセージの id。通知まで終えたら取りこぼし確認の基準として記録する。
    id: Option<String>,
    /// 受信した JSON そのもの。
    text: String,
}

/// code ごとの、取りこぼし確認の状態。
#[derive(Default)]
struct CursorState {
    /// 基準を取ったか。取る前は、どこから拾い直せばよいか判断できない。
    primed: bool,
    /// 通知まで終えた最新の id。`None` は基準を取ったとき履歴が空だったこと。
    /// 津波予報の履歴は平時は空で、これを「最新まで処理済み」と同一視すると、
    /// 切断中に発表された分を起動前の分として捨ててしまう。
    handled: Option<String>,
    /// 通知できなかった最も古い報の id。残っている間は `handled` を進めない。
    /// 進めると、後続の成功がこの報を飛び越して拾い直しの対象から外してしまう。
    failed: Option<String>,
    /// 履歴の確認をやり残しているか。取得に失敗したときに立てる。
    /// これが無いと、確認できなかったこと自体が忘れられ、次の再接続まで
    /// 履歴にしか無い報を拾えない。
    catch_up_pending: bool,
}

/// 通知まで終えた最新のメッセージ id を code ごとに保持する。
///
/// ワーカーと取りこぼし確認の両方から触るため共有する。
#[derive(Clone, Default)]
struct Cursors(Arc<Mutex<HashMap<i32, CursorState>>>);

impl Cursors {
    /// 拾い直しの起点。外側の `None` は未設定（起動直後）。
    fn baseline(&self, code: i32) -> Option<Option<String>> {
        let map = self.lock();
        let state = map.get(&code)?;
        state.primed.then(|| state.handled.clone())
    }

    /// 起点を置き直す。積み残していた失敗も忘れる。
    fn rebase(&self, code: i32, id: Option<String>) {
        let mut map = self.lock();
        let state = map.entry(code).or_default();
        state.primed = true;
        state.handled = id;
        state.failed = None;
    }

    /// 通知まで終えた報を記録する。
    ///
    /// 先に失敗した報が残っている間は起点を進めない。進めるのは、その報自身を
    /// 通知できたとき（穴が埋まったとき）だけ。
    fn record_handled(&self, message: &Incoming) {
        let Some(id) = &message.id else { return };
        if !is_catch_up_target(message.code) {
            return;
        }
        let mut map = self.lock();
        let state = map.entry(message.code).or_default();
        if state.failed.as_deref().is_some_and(|failed| failed != id) {
            return;
        }
        state.failed = None;
        state.handled = Some(id.clone());
    }

    /// 通知できなかった報を記録する。以後、この報を通知できるまで起点は進まない。
    fn record_failed(&self, message: &Incoming) {
        let Some(id) = &message.id else { return };
        if !is_catch_up_target(message.code) {
            return;
        }
        let mut map = self.lock();
        let state = map.entry(message.code).or_default();
        if state.failed.is_none() {
            state.failed = Some(id.clone());
        }
    }

    /// 通知できていない報が残っているか。
    fn has_failures(&self) -> bool {
        self.lock().values().any(|state| state.failed.is_some())
    }

    /// 履歴を見に行く必要があるか。
    ///
    /// 通知できていない報が残っている、まだ基準を取れていない、確認をやり残して
    /// いる、のいずれか。取得や待ち行列の都合で確認できなかった場合も含めるので、
    /// 一度でも失敗すれば成功するまで確認し直す。
    fn needs_catch_up(&self) -> bool {
        if self.has_failures() {
            return true;
        }
        let map = self.lock();
        CATCH_UP_TARGETS.iter().any(|(code, _)| {
            map.get(code)
                .is_none_or(|state| !state.primed || state.catch_up_pending)
        })
    }

    /// 履歴の確認をやり残したことを覚えておく。
    fn mark_catch_up_pending(&self, code: i32) {
        self.lock().entry(code).or_default().catch_up_pending = true;
    }

    /// 履歴の確認が済んだ。
    fn clear_catch_up_pending(&self, code: i32) {
        self.lock().entry(code).or_default().catch_up_pending = false;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i32, CursorState>> {
        self.0.lock().expect("毒された Mutex")
    }
}

/// 再接続時に履歴から拾い直す対象と、その履歴 API の URL。
///
/// 緊急地震速報(556)は含めない。揺れる前に知らせるための情報で、揺れが終わった後に
/// 流すと誤解を招くため。地震情報(551)と津波予報(552)は、遅れて届いても意味がある。
const CATCH_UP_TARGETS: [(i32, &str); 2] = [
    (CODE_JMA_QUAKE, HISTORY_URL),
    (CODE_TSUNAMI, HISTORY_TSUNAMI_URL),
];

/// 再接続時に履歴から拾い直す種別か。
fn is_catch_up_target(code: i32) -> bool {
    CATCH_UP_TARGETS.iter().any(|(target, _)| *target == code)
}

/// 受信テキストを code で振り分けてワーカーへ渡す。
///
/// 待ち行列に上限が無いので、ワーカーが詰まっていても捨てず、受信も止めない。
/// 緊急地震速報は履歴から拾い直せないため、捨てると永久に失われる。
fn dispatch(
    text: &str,
    eew_queue: &EewQueue,
    other_tx: &mpsc::Sender<Job>,
    cursors: &Cursors,
) {
    // 想定外のフォーマットは無視する。
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(code) = value.get("code").and_then(serde_json::Value::as_i64) else {
        return;
    };
    let code = code as i32;
    if !matches!(code, CODE_EEW | CODE_JMA_QUAKE | CODE_TSUNAMI) {
        return;
    }
    let message = Incoming {
        code,
        id: message_id(&value).map(str::to_string),
        text: text.to_string(),
    };

    if code == CODE_EEW {
        // eventId が無い報は差し替え対象を特定できないため扱わない（handle_eew も同じ）。
        let Some(event_id) = event_id(&value) else {
            return;
        };
        eew_queue.push(event_id.to_string(), message);
        return;
    }
    enqueue(other_tx, cursors, message);
}

/// 緊急地震速報の eventId。同一地震の続報を束ねる。
fn event_id(value: &serde_json::Value) -> Option<&str> {
    value
        .get("issue")?
        .get("eventId")?
        .as_str()
        .filter(|id| !id.is_empty())
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
/// `previous` が履歴に無い場合は None を返す。どこまで届いていたのか分からず、
/// 全件を通知すると過去の地震を大量に投稿してしまうため、呼び出し側で基準を
/// 取り直す（拾い直しは諦める）。
fn missed_since<'a>(
    items: &'a [serde_json::Value],
    previous: &str,
) -> Option<Vec<&'a serde_json::Value>> {
    let mut missed: Vec<&serde_json::Value> = items
        .iter()
        .take_while(|v| message_id(v) != Some(previous))
        .collect();
    if missed.len() == items.len() {
        return None;
    }
    missed.reverse();
    Some(missed)
}

/// 履歴と基準から決めた、拾い直しの方針。
enum CatchUp<'a> {
    /// 拾い直さず、基準を取り直すだけ。
    Rebase,
    /// 拾い直す対象（古い順）。
    Missed(Vec<&'a serde_json::Value>),
}

/// 履歴（新しい順）と現在の基準から、拾い直す対象を決める。
fn plan_catch_up<'a>(
    items: &'a [serde_json::Value],
    previous: Option<Option<String>>,
) -> CatchUp<'a> {
    match previous {
        // 起動直後。今ある最新を基準にするだけで、起動前の分は通知しない。
        None => CatchUp::Rebase,
        // 基準を取ったとき履歴が空だった。今ある分はすべて、その後に流れたもの。
        Some(None) => CatchUp::Missed(items.iter().rev().collect()),
        Some(Some(previous)) => match missed_since(items, &previous) {
            Some(missed) => CatchUp::Missed(missed),
            // 基準が履歴に無い。どこまで届いていたか分からず、全件を流すと
            // 過去の分を大量に投稿してしまうため、基準を取り直すだけにする。
            None => CatchUp::Rebase,
        },
    }
}

/// 切断中に流れた分を、拾い直す対象すべてについて履歴から回収する。
async fn catch_up_all(http: &reqwest::Client, tx: &mpsc::Sender<Job>, cursors: &Cursors) {
    // 待ち行列に残っているライブの報を先に処理させる。残したまま古い履歴の報を
    // 積むと、新しい内容が古い内容へ差し戻されてしまう。空にできなければ順序を
    // 崩さないようこの回は見送り、やり残しとして覚えておく（覚えないと、次の
    // 再接続まで履歴にしか無い報を拾えない）。
    if !drain(tx).await {
        for (code, _) in CATCH_UP_TARGETS {
            cursors.mark_catch_up_pending(code);
        }
        return;
    }
    for (code, url) in CATCH_UP_TARGETS {
        catch_up(http, tx, cursors, code, url).await;
    }
}

/// 切断中に流れた1種別を履歴 API から拾い直してワーカーへ渡す。
///
/// 起動直後（基準が無い）は基準を記録するだけで何も通知しない。
/// そうしないと、起動のたびに過去の地震や津波予報をまとめて投稿してしまう。
async fn catch_up(
    http: &reqwest::Client,
    tx: &mpsc::Sender<Job>,
    cursors: &Cursors,
    code: i32,
    url: &str,
) {
    let items = match tokio::time::timeout(CATCH_UP_TIMEOUT, fetch_history(http, url)).await {
        Ok(Ok(items)) => items,
        // 取得できなかったことを覚えておく。覚えないと、基準を持っている種別では
        // 確認し直す理由が無くなり、履歴にしか無い報を次の再接続まで拾えない。
        Ok(Err(e)) => {
            warn!(error = %e, code, "取りこぼしの確認に失敗しました");
            cursors.mark_catch_up_pending(code);
            return;
        }
        Err(_) => {
            warn!(code, "取りこぼしの確認が時間内に終わりませんでした");
            cursors.mark_catch_up_pending(code);
            return;
        }
    };

    // 履歴は新しい順に並んでいる。
    let newest = items.first().and_then(message_id).map(str::to_string);
    let previous = cursors.baseline(code);
    let first_time = previous.is_none();

    // ここまで来れば履歴は見られた。
    cursors.clear_catch_up_pending(code);

    let missed = match plan_catch_up(&items, previous) {
        CatchUp::Rebase => {
            cursors.rebase(code, newest);
            if first_time {
                info!(code, "起動時の基準を記録しました（起動前の分は通知しません）");
            } else {
                warn!(code, "基準が履歴に見つからないため拾い直しを諦め、基準を取り直します");
            }
            return;
        }
        CatchUp::Missed(missed) => missed,
    };
    if missed.is_empty() {
        return;
    }

    info!(code, count = missed.len(), "切断中に流れた分を拾い直します");
    for item in missed {
        let message = Incoming {
            code,
            id: message_id(item).map(str::to_string),
            text: item.to_string(),
        };
        enqueue(tx, cursors, message);
    }
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

    // 同一発表の再送を除去する（id で重複判定）。記録は通知できた後に行う。
    // 送信前に記録すると、失敗した発表が拾い直しでも重複扱いになり永久に失われる。
    let id = tsunami.id.clone();
    if !id.is_empty() && seen.contains(&id) {
        return Ok(());
    }

    if tsunami.cancelled {
        info!("津波予報の解除を受信");
        let payload = discord::build_tsunami_payload(&tsunami, is_test);
        discord::send(http, &config.webhook_url, &payload, None).await?;
        seen.mark(&id);
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
        // 通知対象外と判断できたので、処理済みとして記録する。
        seen.mark(&id);
        return Ok(());
    }

    info!(areas = tsunami.areas.len(), "津波予報を検出");
    let payload = discord::build_tsunami_payload(&tsunami, is_test);
    discord::send(http, &config.webhook_url, &payload, None).await?;
    seen.mark(&id);
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

    /// 新しい順に並んだ履歴を作る。
    fn history(ids: &[&str]) -> Vec<serde_json::Value> {
        ids.iter()
            .map(|id| serde_json::json!({ "id": id, "code": 551 }))
            .collect()
    }

    fn ids_of(items: &[&serde_json::Value]) -> Vec<String> {
        items
            .iter()
            .map(|v| message_id(v).unwrap().to_string())
            .collect()
    }

    /// 現在の基準で拾い直しの対象を求める（地震情報のみ）。
    fn plan<'a>(items: &'a [serde_json::Value], cursors: &Cursors) -> Vec<&'a serde_json::Value> {
        match plan_catch_up(items, cursors.baseline(CODE_JMA_QUAKE)) {
            CatchUp::Missed(missed) => missed,
            CatchUp::Rebase => panic!("基準があるので拾い直しの対象になるはず"),
        }
    }

    fn incoming(code: i32, id: &str) -> Incoming {
        Incoming {
            code,
            id: Some(id.to_string()),
            text: String::new(),
        }
    }

    /// 待ち行列から次のメッセージを取り出す（区切りは想定しない）。
    fn next_message(rx: &mut mpsc::Receiver<Job>) -> Option<Incoming> {
        match rx.try_recv().ok()? {
            Job::Message(message) => Some(message),
            Job::Barrier(_) => panic!("ここでは区切りを流していない"),
        }
    }

    #[test]
    fn dispatch_routes_by_code() {
        let eew_queue = EewQueue::default();
        let (other_tx, mut other_rx) = mpsc::channel(4);
        let cursors = Cursors::default();

        let eew = r#"{"code":556,"_id":"e1","issue":{"eventId":"ev1"}}"#;
        dispatch(eew, &eew_queue, &other_tx, &cursors);
        dispatch(r#"{"code":551,"_id":"q1"}"#, &eew_queue, &other_tx, &cursors);
        dispatch(r#"{"code":552,"_id":"t1"}"#, &eew_queue, &other_tx, &cursors);
        // 対象外の code、壊れた JSON、eventId の無い緊急地震速報は捨てる。
        dispatch(r#"{"code":555}"#, &eew_queue, &other_tx, &cursors);
        dispatch("not json", &eew_queue, &other_tx, &cursors);
        dispatch(r#"{"code":556,"_id":"e2"}"#, &eew_queue, &other_tx, &cursors);

        // 緊急地震速報は地震情報と別の待ち行列に入り、地図生成に待たされない。
        assert_eq!(eew_queue.lock().len(), 1);
        assert_eq!(eew_queue.lock()[0].event_id, "ev1");

        let quake = next_message(&mut other_rx).unwrap();
        assert_eq!((quake.code, quake.id.as_deref()), (CODE_JMA_QUAKE, Some("q1")));
        let tsunami = next_message(&mut other_rx).unwrap();
        assert_eq!((tsunami.code, tsunami.id.as_deref()), (CODE_TSUNAMI, Some("t1")));
        assert!(next_message(&mut other_rx).is_none());
    }

    #[test]
    fn missed_since_returns_newer_items_oldest_first() {
        // 履歴は新しい順。WebSocket は `_id`、履歴 API は `id` で同じ値を返す。
        let items = history(&["n3", "n2", "n1", "seen", "older"]);

        let missed = missed_since(&items, "seen").unwrap();
        assert_eq!(ids_of(&missed), ["n1", "n2", "n3"]);

        // 最新まで処理済みなら拾い直すものは無い。
        assert!(missed_since(&items, "n3").unwrap().is_empty());

        // 基準が履歴に無い場合は、どこまで届いていたか分からないので拾い直さない。
        // 全件を流すと過去の地震を大量に投稿してしまう。
        assert!(missed_since(&items, "unknown").is_none());
    }

    #[test]
    fn cursor_advances_only_for_notified_messages() {
        let cursors = Cursors::default();
        for code in [CODE_JMA_QUAKE, CODE_TSUNAMI] {
            cursors.rebase(code, None);
        }

        // 通知まで終えた分だけ記録する（ワーカーは失敗時に呼ばない）。
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q1"));
        cursors.record_handled(&incoming(CODE_TSUNAMI, "t1"));
        // 緊急地震速報は拾い直しの対象外なので基準を持たない。
        cursors.record_handled(&incoming(CODE_EEW, "e1"));

        assert_eq!(cursors.baseline(CODE_JMA_QUAKE), Some(Some("q1".to_string())));
        assert_eq!(cursors.baseline(CODE_TSUNAMI), Some(Some("t1".to_string())));
        assert_eq!(cursors.baseline(CODE_EEW), None);
    }

    #[test]
    fn failed_notification_stays_in_the_catch_up_range() {
        // 送信に失敗した報は基準にならないので、次の履歴確認で拾い直される。
        // 待ち行列に入れた時点で基準を進めると、この経路で通知が永久に失われる。
        let items = history(&["q3", "q2", "q1", "base"]);
        let cursors = Cursors::default();
        cursors.rebase(CODE_JMA_QUAKE, Some("base".to_string()));

        let missed = plan(&items, &cursors);
        assert_eq!(ids_of(&missed), ["q1", "q2", "q3"]);

        // q1 の通知だけ成功した状態。残りは次回も対象に残る。
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q1"));
        assert_eq!(ids_of(&plan(&items, &cursors)), ["q2", "q3"]);

        // 最新まで通知できたら拾い直すものは無くなる。
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q2"));
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q3"));
        assert!(plan(&items, &cursors).is_empty());
    }

    #[test]
    fn eew_queue_keeps_only_the_latest_report_per_event() {
        // 続報は前の報を差し替える。処理が遅れても投稿するのは常に最新の予想で、
        // 古い続報を後から投稿し直すことがない。緊急地震速報は履歴から拾い直さない
        // ため捨てられないが、この方式なら捨てずに長さを抑えられる。
        let queue = EewQueue::default();
        let (other_tx, _other_rx) = mpsc::channel(4);
        let cursors = Cursors::default();

        for serial in 1..=50 {
            let text = format!(
                r#"{{"code":556,"_id":"e{serial}","issue":{{"eventId":"ev1","serial":"{serial}"}}}}"#
            );
            dispatch(&text, &queue, &other_tx, &cursors);
        }
        // 別の地震は別の枠になる。
        dispatch(
            r#"{"code":556,"_id":"x1","issue":{"eventId":"ev2"}}"#,
            &queue,
            &other_tx,
            &cursors,
        );

        assert_eq!(queue.lock().len(), 2, "地震ごとに1件だけ持つ");
        assert_eq!(queue.lock()[0].event_id, "ev1");
        assert_eq!(queue.lock()[0].message.id.as_deref(), Some("e50"), "最新の報が残る");
    }

    #[test]
    fn eew_queue_bounds_the_number_of_events() {
        // 同時にこれだけの地震が進行することは無いが、際限なく積み上がらないこと。
        let queue = EewQueue::default();
        for event in 0..(EEW_QUEUE_CAPACITY + 10) {
            queue.push(format!("ev{event}"), incoming(CODE_EEW, "e"));
        }
        assert_eq!(queue.lock().len(), EEW_QUEUE_CAPACITY);
        // 捨てられるのは最も古い＝最も価値の低い報。
        assert_eq!(queue.lock()[0].event_id, "ev10");
    }

    #[tokio::test]
    async fn eew_queue_waits_for_the_next_report() {
        let queue = EewQueue::default();
        let waiting = tokio::spawn({
            let queue = queue.clone();
            async move { queue.pop().await.event_id }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        queue.push("ev1".to_string(), incoming(CODE_EEW, "e1"));
        assert_eq!(waiting.await.unwrap(), "ev1");
    }

    #[test]
    fn full_queue_records_a_failure_so_the_report_is_picked_up_again() {
        // 地震情報・津波予報は履歴から拾い直せるので、満杯なら捨ててよい。
        // ただし失敗として記録しないと、基準が飛び越して永久に失われる。
        let items = history(&["q2", "q1", "base"]);
        let cursors = Cursors::default();
        cursors.rebase(CODE_JMA_QUAKE, Some("base".to_string()));

        // 受け手がいないまま満杯にする。
        let (other_tx, _other_rx) = mpsc::channel(1);
        enqueue(&other_tx, &cursors, incoming(CODE_JMA_QUAKE, "q1"));
        enqueue(&other_tx, &cursors, incoming(CODE_JMA_QUAKE, "q2"));

        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q1"));
        assert_eq!(ids_of(&plan(&items, &cursors)), ["q1", "q2"]);
    }

    #[tokio::test]
    async fn barrier_reports_when_the_queue_is_drained() {
        // 拾い直しの前に、待ち行列に残っているライブの報を処理させるための目印。
        let (tx, mut rx) = mpsc::channel::<Job>(4);
        let worker = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(job) = rx.recv().await {
                match job {
                    Job::Message(message) => seen.push(message.id.unwrap_or_default()),
                    Job::Barrier(done) => {
                        let _ = done.send(());
                        return seen;
                    }
                }
            }
            seen
        });

        let cursors = Cursors::default();
        for id in ["q1", "q2"] {
            enqueue(&tx, &cursors, incoming(CODE_JMA_QUAKE, id));
        }
        assert!(drain(&tx).await, "空にできたと報告すること");

        // 区切りに達した時点で、先に入れた分は処理し終えている。
        assert_eq!(worker.await.unwrap(), ["q1", "q2"]);
    }

    #[tokio::test]
    async fn tsunami_is_not_marked_seen_when_sending_fails() {
        // 送信前に記録すると、失敗した発表が再送や履歴からの拾い直しでも重複扱いに
        // なり、通知が永久に失われる。
        let config = Config {
            webhook_url: "http://127.0.0.1:1/failing-webhook".to_string(),
            ws_url: String::new(),
            region_min_scales: HashMap::new(),
            other_min_scale: 10,
            attach_map: false,
            tile_url_template: String::new(),
        };
        // macOS ではプロキシの自動検出が panic することがあるため無効にする。
        // 送信先は 127.0.0.1 なのでプロキシは不要。
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let text = r#"{"code":552,"_id":"t1","areas":[{"grade":"Watch","name":"三陸沿岸"}]}"#;
        let mut seen = SeenIds::default();

        let result = handle_tsunami(&config, &http, text, false, &mut seen).await;
        assert!(result.is_err(), "送信できなければエラーになるはず");
        assert!(!seen.contains("t1"), "送信に失敗した発表は記録しない");
    }

    #[test]
    fn later_success_does_not_skip_over_a_failed_report() {
        // q1 成功 → q2 失敗 → q3 成功。q3 まで基準を進めると q2 が拾い直しの
        // 対象から外れ、通知が永久に失われる。
        let items = history(&["q3", "q2", "q1", "base"]);
        let cursors = Cursors::default();
        cursors.rebase(CODE_JMA_QUAKE, Some("base".to_string()));

        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q1"));
        cursors.record_failed(&incoming(CODE_JMA_QUAKE, "q2"));
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q3"));
        assert_eq!(ids_of(&plan(&items, &cursors)), ["q2", "q3"]);

        // 拾い直しで q2 も失敗し続ける間は、対象から外れない。
        cursors.record_failed(&incoming(CODE_JMA_QUAKE, "q2"));
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q3"));
        assert_eq!(ids_of(&plan(&items, &cursors)), ["q2", "q3"]);

        // q2 を通知できたら穴が埋まり、その先へ進める。
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q2"));
        assert_eq!(ids_of(&plan(&items, &cursors)), ["q3"]);
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q3"));
        assert!(plan(&items, &cursors).is_empty());
    }

    #[test]
    fn pending_failure_is_visible_until_the_gap_is_filled() {
        // 接続が続いていても拾い直しに行くかどうかの判断に使う。
        let cursors = Cursors::default();
        cursors.rebase(CODE_TSUNAMI, None);
        assert!(!cursors.has_failures());

        cursors.record_failed(&incoming(CODE_TSUNAMI, "t1"));
        assert!(cursors.has_failures());

        // 別の報を通知できても、穴が埋まるまでは残り続ける。
        cursors.record_handled(&incoming(CODE_TSUNAMI, "t2"));
        assert!(cursors.has_failures());

        cursors.record_handled(&incoming(CODE_TSUNAMI, "t1"));
        assert!(!cursors.has_failures());
    }

    #[test]
    fn failed_history_lookup_keeps_asking_for_a_catch_up() {
        // 履歴を取れなかったこと自体を覚えておかないと、基準を持っている種別では
        // 確認し直す理由が無くなり、履歴にしか無い報を次の切断まで拾えない。
        let cursors = Cursors::default();
        for (code, _) in CATCH_UP_TARGETS {
            cursors.rebase(code, Some("base".to_string()));
        }
        assert!(!cursors.needs_catch_up());

        cursors.mark_catch_up_pending(CODE_TSUNAMI);
        assert!(cursors.needs_catch_up(), "取得できるまで確認し直す");

        cursors.clear_catch_up_pending(CODE_TSUNAMI);
        assert!(!cursors.needs_catch_up());
    }

    #[test]
    fn missing_baseline_keeps_asking_for_a_catch_up() {
        // 初回の履歴取得に失敗すると基準が空のままになる。放置すると次の再接続まで
        // 拾い直しに行かず、その間の取りこぼしを拾えない。
        let cursors = Cursors::default();
        assert!(cursors.needs_catch_up(), "基準が無いなら取りに行く");

        // ライブの報を処理しただけでは基準は確立しない。
        cursors.record_handled(&incoming(CODE_JMA_QUAKE, "q1"));
        assert!(cursors.needs_catch_up());

        // 履歴を取れて初めて基準が定まり、以後は失敗が無い限り取りに行かない。
        for (code, _) in CATCH_UP_TARGETS {
            cursors.rebase(code, None);
        }
        assert!(!cursors.needs_catch_up());
    }

    #[test]
    fn startup_does_not_replay_past_reports() {
        // 起動直後は基準を取るだけ。過去の分をまとめて投稿しない。
        let items = history(&["q2", "q1"]);
        assert!(matches!(plan_catch_up(&items, None), CatchUp::Rebase));
    }

    #[test]
    fn empty_history_at_startup_does_not_swallow_later_reports() {
        // 津波予報の履歴は平時は空。空を「最新まで処理済み」と同一視すると、
        // 切断中に発表された分を起動前の分として捨ててしまう。
        let cursors = Cursors::default();
        assert!(matches!(
            plan_catch_up(&[], cursors.baseline(CODE_TSUNAMI)),
            CatchUp::Rebase
        ));
        cursors.rebase(CODE_TSUNAMI, None);

        // その後に発表された分はすべて取りこぼし扱いになる。
        let items = history(&["t2", "t1"]);
        let CatchUp::Missed(missed) = plan_catch_up(&items, cursors.baseline(CODE_TSUNAMI)) else {
            panic!("拾い直しの対象になるはず");
        };
        assert_eq!(ids_of(&missed), ["t1", "t2"]);
    }

    #[test]
    fn unknown_cursor_rebases_instead_of_replaying_everything() {
        // 基準が履歴に無い場合、どこまで届いていたか分からない。全件を流すと
        // 過去の分を大量に投稿してしまうため、拾い直さず基準を取り直す。
        let items = history(&["q3", "q2", "q1"]);
        let previous = Some(Some("知らない基準".to_string()));
        assert!(matches!(plan_catch_up(&items, previous), CatchUp::Rebase));
    }

    #[test]
    fn catch_up_targets_exclude_eew() {
        // 地震情報と津波予報は遅れて届いても意味があるが、緊急地震速報は
        // 揺れる前に知らせる情報なので、終わった後に流さない。
        assert!(is_catch_up_target(CODE_JMA_QUAKE));
        assert!(is_catch_up_target(CODE_TSUNAMI));
        assert!(!is_catch_up_target(CODE_EEW));

        // 拾い直す種別には履歴 API の URL が対応している。
        for (code, url) in CATCH_UP_TARGETS {
            assert!(url.contains(&format!("codes={code}")), "{code} の URL: {url}");
        }
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
