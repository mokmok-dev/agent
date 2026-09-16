# opencode の SQLite とセッションストレージの役割分担

> 出典調査: [anomalyco/opencode](https://github.com/anomalyco/opencode) を読んで、セッションごとの SQLite と shared SQLite の役割の違いを参考文献として抽出したメモ。
>
> 検証方法: ローカルにインストール済みの opencode v1.18.22(Nix 由来)の実ファイル・ログ・データディレクトリ、および本リポジトリ `agentd` のソースコードを一次情報として確認した。GitHub リポジトリ本体のソースツリーはローカルに存在しないため、リポジトリ本体については「インストール版の振る舞い」として記す。

## 結論

**「セッションごとの SQLite」という前提は opencode には当てはまらない。** opencode は SQLite を常に1つだけ(共有 DB)持ち、セッション固有データはセッションごとの JSON ファイルとして保存する。

| レイヤ | opencode (インストール版 v1.18.22) | agentd (本リポジトリ) |
|---|---|---|
| セッション横断の永続層 | `~/.local/share/opencode/opencode.db` 1本を全セッションで共有 | 1本のグローバルイベントログ (`/tmp/mokmokd-events.jsonl` デフォルト、JSONL) |
| セッション固有データ | `storage/session_diff/ses_<id>.json` に1セッション1ファイル | (設計のみ) セッションストア案は現在リポジトリに存在しない |
| セッション状態の保持 | SQLite は保持しない(差分は JSON) | イベントログは保持しない(append-only 監査ログ) |

→ 「共有の永続層 = セッション横断」「セッション固有データ = セッションごとのファイル」という分担が答え。agentd 側の共有層は SQLite ではなく JSONL イベントログになったが、分担の構造は同じ。

## opencode 本体: DB は共有1本、セッションデータは JSON

- 実行中の opencode サーバが開く SQLite は `~/.local/share/opencode/opencode.db`(`-shm`/`-wal` サイドカー付き)の1本だけ。ログにセッション単位の DB は一切現れない。
- スキーマはサーバにバンドルされたマイグレーション群(`loose_psylocke`, `import_legacy_credentials`, `workspace_domain`, `execution_claim_attempts`, `session_inbox`, `worktree`)で管理される。
- セッションが変更したファイルの差分は `storage/session_diff/ses_<id>.json` として1セッション1ファイルで保存される。
- 各エントリは `{file, patch, additions, deletions, status}` 形式(検証済みの実例):

```json
{
  "file": "Cargo.lock",
  "patch": "Index: Cargo.lock\n===================================================================\n--- Cargo.lock\n+++ Cargo.lock\n@@ -1,5945 +1,6147 @@\n ...",
  "additions": 215,
  "deletions": 13,
  "status": "modified"
}
```

## agentd のイベントログ(グローバルな JSONL ログ)

- 実装は `agentd-events/src/log.rs` の `EventLog`。1本の JSONL ファイルをデーモン全体・全セッションで共有する、append-only のイベントログとして動作する。
- 1行が CloudEvents 1.0 エンベロープ JSON そのもの。位置は1始まりの行番号(`Seq`)で、スキーマレスなので拡張属性はそのまま保存される。
- 履歴検索はログを直接読む(`jq`/`rg`/DuckDB)。状態を持つ読み取りモデルは `agentd_events::projection` でログから replay する。
- ファイルはデフォルトで `/tmp/mokmokd-events.jsonl`、`--log-path` で変更可能(`agentd/src/main.rs` の `Serve` サブコマンド定義)。open() 時に親ディレクトリが自動作成される。

## 並行性・単一ライタ保証・耐久性

- ログファイルとシーケンス番号は単一の専用ライタスレッド(スレッド名 `"eventlog"`)だけが所有する。async の publisher は bounded mpsc チャネル(容量 1024)でイベントを送り、ライタがバッチ単位で追記して1回 fsync する(group commit)。async ランタイムからファイルに直接触る経路はない。
- 単一ライタ構造は Unix ソケット排他によるデーモンの単一インスタンス制約でも構造的に保証される。既存ソケットが生きていれば `AlreadyRunning` エラーを返して起動しない(`agentd/src/server.rs`)。
- 読み書きは append-only。起動時の復旧は完全な行をストリームして数え、クラッシュ由来の末尾半端行を切り捨て、シーケンス番号を再開する。
- agentd のイベントログはグローバルな監査ログであり、セッション状態は保持しない。

## 役割の違い

2つの保存層は役割が完全に分かれる:

1. **共有イベントログ** はセッションを横断する単一の永続層。
   - opencode 本体: 全セッション共通の共有 SQLite DB。
   - agentd: 全セッションのイベントを時系列で残す append-only の JSONL 監査ログ。
2. **セッションに紐づく作業内容**(ファイル差分)はセッションごとの JSON ファイルが担う。
   - opencode: `storage/session_diff/ses_<id>.json`。
   - データの保持単位は「DB は全体で1つ、ファイルはセッションごと」と明確に使い分けられている。

## 検証上の注意(不確実性)

- GitHub の opencode リポジトリ本体(source tree / 該当モジュールのファイルパス)はローカルに存在せず、リポジトリ本体の一次情報から直接引用できるのはインストール版 v1.18.22 の振る舞いまで。開発ブランチ等でセッションごと SQLite を実装している可能性は否定できていない。
- ローカル検証時に `docs/session-store.md` が存在した(セッション開始時の untracked ファイル)が、現時点のリポジトリには存在しない。検証エージェント間でこのファイルの有無に関して矛盾した観測が残っており、「セッションストアの設計文書」の扱いは要確認。本リポジトリのソースコードにはセッション概念の実装は存在しない(グローバルイベントログのみ)。
- セッション差分のスキーマ詳細(`CREATE TABLE` DDL 等)はコンパイル済みバイナリ内にあり、sqlite3 CLI での直接確認には至っていない。
- バックアップ戦略は repo のどこにも明示されていない(「backup」への言及なし)。破棄・アーカイブの方針のみ英語で記述されている。