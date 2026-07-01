# 地震bot

[P2P地震情報 JSON API v2](https://www.p2pquake.net/develop/json_api_v2/) の WebSocket を購読し、日本国内で強い揺れの地震が発生したときに、震源地・マグニチュード・震度を **Discord Webhook** へ通知します。座標が判明している場合は震源地・各地の震度（緊急地震速報は予想震度）をプロットした地図画像も添付します。

**速報と確定の2段構え（hybrid）** で通知します。

- ⚡ **緊急地震速報（code 556）**: 地震発生直後（第1報は発生の約10秒後）に **予想震度** で速報。
- 🚨 **地震情報（code 551）**: 各地の震度が集計された後（震度速報で約1.5分後〜）に **観測震度** で確定通知。
- 🌊 **津波予報（code 552）**: 津波注意報・警報・大津波警報の発表/解除を通知（震度しきい値とは無関係に常時通知）。

## 通知条件

デフォルトは全国どこでも震度5強以上で通知します。地方ごとに下限を設定して変更できます。
この条件は 551 の観測震度・556 の予想震度の**両方**に適用されます。

- いずれかの地点（地域）の震度が、その地点の属する**地方の下限**以上 → 通知
- 地方ごとの下限は環境変数で設定でき、**未設定の地方は `OTHER_MIN_SCALE`（既定 50 = 震度5強）にフォールバック**します。
- 既定ではどの地方も特別扱いせず `OTHER_MIN_SCALE` 相当です。例えば関東・近畿を震度4で通知したい場合は `KANTO_MIN_SCALE=40` `KINKI_MIN_SCALE=40` を設定します。

### 地方区分と環境変数

| 地方 | 環境変数 | 既定 |
| --- | --- | --- |
| 北海道 | `HOKKAIDO_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 東北 | `TOHOKU_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 関東 | `KANTO_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 中部 | `CHUBU_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 近畿 | `KINKI_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 中国 | `CHUGOKU_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 四国 | `SHIKOKU_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |
| 九州（沖縄含む） | `KYUSHU_MIN_SCALE` | 未設定→`OTHER_MIN_SCALE` |

震度スケール値: `10`=1, `20`=2, `30`=3, `40`=4, `45`=5弱, `50`=5強, `55`=6弱, `60`=6強, `70`=7。

> **緊急地震速報(556)の制約**: P2P地震情報の 556 は緊急地震速報の **「警報」のみ**（気象庁が最大震度5弱以上を予想した場合のみ発表）です。震度4程度の弱い揺れは 556 では速報されず、551 の計測後通知のみになります。また 556 は予想値のため、続報で内容が変わったり取り消されたりすることがあります（本botは第1報で通知し、取消も通知します）。

## セットアップ

1. Rust（stable）をインストール: https://rustup.rs/
2. Discord でチャンネルの Webhook URL を発行（サーバー設定 → 連携サービス → ウェブフック）。
3. 設定ファイルを用意:

   ```sh
   cp .env.example .env
   # .env を編集して DISCORD_WEBHOOK_URL を設定
   ```

## ビルドと実行

```sh
cargo run --release
```

テスト（通知判定ロジック）:

```sh
cargo test
```

実通知の動作確認（過去のデータを1件取得して Discord へ送信）:

```sh
cargo run -- --test          # 地震情報(551) の確定通知をテスト送信
cargo run -- --test-prompt   # 震源未確定の報（観測県マーカーマップ）をテスト送信
cargo run -- --test-eew      # 緊急地震速報(556) の速報通知をテスト送信
cargo run -- --test-tsunami  # 津波予報(552) の通知をテスト送信（履歴に津波予報がある場合）
```

## 設定（環境変数）

| 変数 | 既定値 | 説明 |
| --- | --- | --- |
| `DISCORD_WEBHOOK_URL` | （必須） | Discord Webhook URL |
| `<地方>_MIN_SCALE` | 未設定 | 地方ごとの通知する最小震度スケール（未設定は `OTHER_MIN_SCALE`。上記「地方区分と環境変数」を参照） |
| `OTHER_MIN_SCALE` | `50` | 下限が未設定の地方・地域で通知する最小震度スケール |
| `ATTACH_MAP` | `true` | 地図画像を添付するか |
| `TILE_URL_TEMPLATE` | 地理院タイル(白地図 blank) | 地図タイルの URL テンプレート |
| `P2PQUAKE_WS_URL` | `wss://api.p2pquake.net/v2/ws` | WebSocket エンドポイント |
| `RUST_LOG` | `info` | ログレベル |

`.env` が無い場合は OS の環境変数を参照します。

## 構成

```
src/
  main.rs       … エントリポイント。WebSocket購読・再接続・通知の制御
  config.rs     … 環境変数の読み込み
  model.rs      … P2P地震情報 API のレスポンス型(serde)
  intensity.rs  … 震度変換・通知条件の判定（単体テストあり）
  geo.rs        … 観測点(市区町村)・都道府県の座標テーブルとマーカー変換
  mapgen.rs     … 震源地・各地の震度をプロットした地図WebPの生成(staticmap、既定は地理院タイル)
  discord.rs    … Discord embed の組み立てと Webhook 送信(multipart)
  data/observation_points.tsv … 観測点名→座標のデータ（気象庁公表データを基に集約、CC BY 4.0）
```

## 動作の概要

1. `wss://api.p2pquake.net/v2/ws` に接続し、流れてくる JSON を1件ずつ処理。
2. `code` で振り分け：`556`（緊急地震速報）・`551`（地震情報）・`552`（津波予報）を解析。
3. 556 は第1報で速報（予想震度）。同一地震（`eventId`）の続報は抑制し、取消も通知。
4. 551 は通知条件を満たせば確定通知（観測震度）。津波情報があればタイトルに🌊。
5. 552 は津波注意報以上を通知。同一発表（`id`）の再送は抑制し、解除も通知。
6. 通知条件を満たす場合のみ地図画像（座標があれば）を生成し、Discord Webhook へ embed + 画像を送信。
7. 切断時は指数バックオフ（最大60秒）で自動再接続。

## 注意事項

- **出典の表示（二次利用規約の遵守）**: 地震・津波データは気象庁発表（CC BY 4.0、[P2P地震情報](https://www.p2pquake.net/)経由）です。[P2P地震情報の二次利用規約](https://www.p2pquake.net/secondary_use/)に従い、本botはすべての通知のフッターに「出典: 気象庁」を明記します（地図添付時は「地理院タイル」も併記）。商用・非商用問わず利用できますが、情報の正確性は保証されません。
- **地図タイルの利用規約**: 既定は国土地理院の白地図(blank、日本全国・zoom 5〜14)タイルです。利用にあたっては出典「地理院タイル」の表示が必要で、本botは地図添付時に通知へ出典と[地理院タイル一覧ページ](https://maps.gsi.go.jp/development/ichiran.html)へのリンクを明記します。高頻度・大量利用の場合は[地理院タイルの利用規約](https://maps.gsi.go.jp/development/ichiran.html)を確認してください。OpenStreetMap など別のタイルへ差し替える場合は `TILE_URL_TEMPLATE` に `{z}/{x}/{y}` 形式の URL を設定します（例: `https://tile.openstreetmap.org/{z}/{x}/{y}.png`。OSM 公式タイルは[利用ポリシー](https://operations.osmfoundation.org/policies/tiles/)に注意）。
- P2P地震情報は気象庁発表をもとにした第三者サービスです。緊急地震速報（予報・警報）そのものではなく、揺れの「予想/観測」情報を扱います。重大用途には公式情報源も併用してください。
- 震源座標が不明な情報（`latitude`/`longitude` が無効値）では地図を添付せずテキストのみで通知します。

## 常時稼働

`systemd` の例（`/etc/systemd/system/quake-alert-bot.service`）:

```ini
[Unit]
Description=Quake Alert Bot
After=network-online.target

[Service]
WorkingDirectory=/opt/quake-alert-bot
EnvironmentFile=/opt/quake-alert-bot/.env
ExecStart=/opt/quake-alert-bot/target/release/quake-alert-bot
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

## ライセンス

本プロジェクトのソースコードは [Unlicense](LICENSE)（パブリックドメインへの献納）です。著作権を主張せず、商用・非商用を問わず自由に利用・改変・再配布できます。

ただし、**依存しているサードパーティ・ライブラリはそれぞれのライセンスに従います**。本botのバイナリには多数の Rust crate が含まれ、主なライセンスは次のとおりです（全件は `cargo metadata` で確認できます）。

- 大半: `MIT` / `Apache-2.0`
- その他: `BSD-2-Clause` / `BSD-3-Clause`（tiny-skia など描画系）、`ISC`、`Zlib`、`Unicode-3.0`（icu 系）、`MPL-2.0`（attohttpc, webpki-roots）など

**バイナリを配布する場合**は、これら依存ライブラリの著作権表示・ライセンス文を保持・同梱する必要があります（`MPL-2.0` のコンポーネントは、該当部分のソース入手先を示す必要があります）。ライセンス表示の一覧は [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) などで生成できます。GPL/AGPL など強いコピーレフトの依存は含まれていません。

なお、配信データ・地図の出典表示（気象庁／地理院タイル）の義務については「注意事項」を参照してください。
