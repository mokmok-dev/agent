# Research Index

外部プロジェクト・設計ドキュメントからの文献調査メモを集める場所。

| 文書 | 対象 | 概要 |
|---|---|---|
| [opencode の SQLite とセッションストレージの役割分担](opencode-sqlite-session-store.md) | anomalyco/opencode (インストール版 v1.18.22) + 本リポジトリ agentd | 共有 SQLite(セッション横断の永続層)とセッション固有データ(JSON ファイル)の分担。<br>セッションごとの SQLite という前提は opencode には当てはまらない。 |
| [session-store.md](../session-store.md) | agentd のセッションストア設計案 | セッションごとの SQLite ストア設計。**現在リポジトリに存在しない(要確認)。** |