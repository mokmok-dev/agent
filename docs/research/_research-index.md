# Research Index

外部プロジェクト・設計ドキュメントからの文献調査メモを集める場所。

| 文書 | 対象 | 概要 |
|---|---|---|
| [opencode の SQLite とセッションストレージの役割分担](opencode-sqlite-session-store.md) | anomalyco/opencode (インストール版 v1.18.22) + 本リポジトリ agentd | 共有の永続層(セッション横断)とセッション固有データ(JSON ファイル)の分担。<br>セッションごとの SQLite という前提は opencode には当てはまらない。agentd 側の共有層は JSONL イベントログ。 |