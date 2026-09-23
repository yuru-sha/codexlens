# codexlens

[English](README.md) | [日本語](README.ja.md)

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/codexlens)

Codex セッションを分析し、繰り返し発生する摩擦を実用的な
`AGENTS.md` の改善案にまとめます。

> Foundation MVP の状況: ローカル取り込み、指示の記録、決定論的な
> レンズ、範囲を限定したレポート、ローカル監視、圧縮 rollout リーダー、
> バージョン付き JSON 出力、安全な `optimize --apply` を実装済みです。
> 再ベースライン [#143](https://github.com/yuru-sha/codexlens/issues/143) は、
> マージ済みの [#156](https://github.com/yuru-sha/codexlens/pull/156) により
> クローズされました。コマンド契約と合成データによる CLI 互換性ゲートは
> 実装済みですが、製品の準備完了を意味するものではありません。準備状況を
> 判断するには、明示的に選択したローカルソースに対して、所有者の許可を
> 得た実データ履歴のスモーク実行が引き続き必要です。
> レポート系コマンドは既定で派生ストアを更新します。`--frozen` を指定すると
> ストアのみを対象とする境界が明確になり、監視は派生ストアと任意の
> カーソルファイルを更新します。適用処理が書き込むのは検証済みの
> 書き込み対象だけです。

## 目的

`codexlens` はローカルで動作するルールベースの Codex ハーネス最適化ツールです。

製品契約は [`docs/specs/product.md`](docs/specs/product.md) にあります。
codexlens は `cclens` に対応する Codex 向けツールとして、範囲を限定した
ヘルスチェックと、インベントリ、オーバーヘッド、使用状況、無駄、
失敗、停滞、プロンプト、SQL/クエリ、最適化の各ビューを提供します。

```text
Codex のローカル状態 + プロジェクト指示
        ↓
      adapter
        ↓
 canonical records → SQLite
        ↓
 failures / corrections / rework / verification /
 knowledge / instructions
        ↓
      findings
        ↓
 doctor / optimize
```

MVP は次のような問いに答えることを目指します。

- どの失敗や修正が繰り返し発生しているか。
- そのとき有効な指示のチェーンが存在していたか。
- どの検証手順が繰り返し抜けているか。
- どのプロジェクト知識がセッションをまたいで再発見されているか。
- どの小さく範囲を絞った指示の変更が、証拠によって裏付けられるか。

MVP はローカルのみで動作し、決定論的で、証拠に基づきます。セッション
データをサービスに送信せず、LLM を必要とせず、検証済みの指示・文書
書き込み対象以外のソースファイルを変更せず、課金額の正確性も主張しません。

## CLI の概要

バイナリは明示的な更新ワークフローと、ユーザーごとの派生 SQLite ストアを
参照する範囲限定のレポートビューを提供します。読み取りビューでは
`--codex-home`、`-s, --store`、`--scope global|project|project:PATH`、
`--include-archived`、`--include-subagents`、`--since`、`--until`、
`--format table|markdown|json`、`--frozen` を指定できます。既定のストアは
`${XDG_STATE_HOME:-~/.local/state}/codexlens/codexlens.db` です。
レポート系コマンドは `--frozen` がなければ選択した Codex home を増分処理します。
進捗と鮮度の診断は stderr に出力され、
JSON stdout はバージョン付きの単一ドキュメントです。読み取り専用レポートは
再現可能な期間境界も受け付けます。出力では、要求期間、観測範囲、ストアの
鮮度を区別します。書き込みを行う例外は明示的な `optimize --apply` で、
検証済みの指示・文書書き込み対象だけを更新できます。

レガシーストアのレポートでは、一時的に移行したコピーを作成する場合があり、
処理後に削除します。`optimize --diff` は差分を表示するために推奨対象の
指示ファイルも読み込みます。

| コマンド | 入力 | 出力の目的 | 読み取り専用の動作 |
| --- | --- | --- | --- |
| `refresh` | 検出した Codex home、rollout/state 入力、指示ファイル | 派生ストアの作成または更新 | 選択した派生ストアだけを書き込み、元入力は変更しない |
| `analyze` | Codex home と派生ストア | すべてのレンズの検出結果 | `--frozen` がなければ選択したストアを更新 |
| `sessions` | Codex home と派生ストア | 範囲を限定したセッション情報とカバレッジ | `--frozen` がなければ選択したストアを更新 |
| `inventory` | Codex home と派生ストア | 設定済みの対象、使用状況、起動時の推定 | `--frozen` がなければ選択したストアを更新 |
| `overhead` | Codex home と派生ストア | 常時読み込まれるコンテキストのコストと残余 | `--frozen` がなければ選択したストアを更新 |
| `usage` | Codex home と派生ストア | ツール、Skills、モデル、プロンプト、サブエージェント、対象の使用状況 | `--frozen` がなければ選択したストアを更新 |
| `waste` | Codex home と派生ストア | 削除・縮小・適用範囲見直しの候補を順位付け | `--frozen` がなければ選択したストアを更新 |
| `failures` | Codex home と派生ストア | 正規化したカテゴリと担当者別の反復失敗 | `--frozen` がなければ選択したストアを更新 |
| `corrections` | Codex home と派生ストア | correction レンズの検出結果 | `--frozen` がなければ選択したストアを更新 |
| `rework` | Codex home と派生ストア | 従来形式の手戻り検出結果 | `--frozen` がなければ選択したストアを更新 |
| `stuck` | Codex home と派生ストア | 範囲を限定した編集・失敗ループと関連パス | `--frozen` がなければ選択したストアを更新 |
| `prompts` | Codex home と派生ストア | steer/correct/question/instruct のパターン | `--frozen` がなければ選択したストアを更新 |
| `verification` | Codex home と派生ストア | verification レンズの検出結果 | `--frozen` がなければ選択したストアを更新 |
| `knowledge` | Codex home と派生ストア | knowledge レンズの検出結果 | `--frozen` がなければ選択したストアを更新 |
| `rediscovery` | Codex home と派生ストア | `knowledge` の別名 | `--frozen` がなければ選択したストアを更新 |
| `instructions` | Codex home と派生ストア | instruction レンズの検出結果 | `--frozen` がなければ選択したストアを更新 |
| `doctor` | Codex home と派生ストア | スコープ別のアクション優先ヘルスサマリー | `--frozen` がなければ選択したストアを更新 |
| `sql` | 既存の派生ストアと SQL/stdin | 範囲限定の読み取り専用テーブル、Markdown、JSON 行 | 更新もストア作成もしない |
| `query` | 既存の派生ストアと SQL/stdin | 範囲限定のテーブル、Markdown、JSON 行 | ストアを読み取り専用で開く。更新も作成もしない |
| `optimize` / `optimize --print` | Codex home と派生ストア | 対話型調査またはブリーフィング | `--frozen` がなければストアを更新し、対象ファイルは変更しない |
| `optimize --diff` | Codex home、派生ストア、対象の指示ファイル | 確度の高い提案差分とスキップ理由 | `--frozen` がなければストアを更新し、対象ファイルは変更しない |
| `optimize --apply --yes` | Codex home、派生ストア、検証済みの指示・文書対象 | 提案を適用し、保持したバックアップと復旧情報を報告 | 確認後に検証済み対象だけを変更 |
| `monitor` | ローカルの rollout JSONL または state SQLite ソース1つ | 範囲限定の増分取り込みとカーソル・状態出力 | ソースは変更せず、派生ストアと任意のカーソルファイルを書き込む |

`doctor` では任意の `--limit COUNT` を使い、スコープごとの検出件数を制限できます。
レポートコマンドはコマンド固有の必須値引数なしで実行でき、各オプションは任意です。
`sql` / `query` は位置引数を省略すると stdin から SQL を読み込みます。`monitor` は
`--source PATH` が必須です。
`sql` は位置引数で SQL 文を1つ受け取るか、stdin から読み込みます。読み取り専用の
単一文だけを受け付け、出力は50列・50行に制限し、エラーに SQL 文を表示しません。
JSON はバージョン付き `sql` エンベロープ内で
`{columns, rows, omitted_column_count, omitted_count}` を使用します。
`query` は同じ契約を持つ明示的な互換エイリアスです。

`optimize` では `--diff`、`--print`、`--apply` のいずれか1つを指定します。
`--diff` と `--print` は助言目的で対象ファイルを変更しません（通常はストアを更新します）。
`--print` は diff フラグなしで
ブリーフィングを出力します。`--apply` には明示的な確認が必要です。
非対話的な実行では、diff を確認したあとに `--yes` を追加します。書き込み対象
全体を検証し、すべてのファイルを再読み込みしてハッシュを再計算します。
成功後もバックアップを保持し、失敗時には一括処理全体をロールバックします。
`analyze` はすべてのレガシーレンズを報告し、個別分析コマンドは型付けされた
ビューをそれぞれの決定論的なレポート形式で報告します。

読み取り専用レポートコマンドに `--format json` を加えると、スキーマバージョン1を
出力します。エイリアスは正規のコマンド名を出力します。すべてのビューは
トップレベルに `scope`、`coverage`、`freshness` のメタデータを持つバージョン付き
エンベロープを使用します。`data` オブジェクトには名前付きの範囲限定フィールドが
含まれます。ストアが存在しない場合や無効な場合は、範囲を限定した実行可能な
エラーを返します。サポート対象の古いスキーマは、一時コピー内でのみ移行され、
指定されたストアは変更されません。

レポート期間には、`Z` または数値の `+HH:MM` / `-HH:MM` オフセットと、任意の
小数秒（最大9桁）を含む完全な RFC3339 タイムスタンプを使用します。時刻は UTC
で比較し、期間は半開区間 `[since, until)` です。どちらかの境界は省略でき、
同じ境界を指定するとアクティビティは選択されません。逆転した境界や無効な
境界の場合は、ストアを読み書きせずに失敗します。相対期間はこのインターフェース
には含まれません。期間で絞り込んだ JSON では、要求・観測境界、含まれる／除外
されたレコード数、不明なレコード／イベント数、`empty` / `complete` / `partial`
の状態が `coverage` オブジェクトに追加されます。
`optimize --apply` は期間フィルターを拒否します。`monitor` は独自の取り込み境界を
保持します。

`refresh` は `--codex-home PATH`（または `--home PATH`）、
`--include-archived`、`--config PATH`、`--store PATH` を受け付けます。
`--codex-home` がなければ、`CODEX_HOME` とプラットフォームの既定値から検出します。
refresh が成功すると、ソースごとの取り込み・スキップのサマリーと記録された
ストア鮮度を出力します。検出と解析の診断は、範囲を明示して出力します。

元入力からストアを作成または更新します。

```bash
$ cargo run -- refresh --codex-home "$CODEX_HOME" --store .codexlens.sqlite
```

adapter は通常と zstd 圧縮 rollout JSONL の両方を読み込みます。レポート処理は
`--frozen` がなければストア更新時に元入力を読み込み、その後ストアから描画します。
`monitor` は明示的なローカル監視の例外です。
rollout または state のソース1つをポーリングし、既存の adapter と正規モデルを
再利用して、派生ストアだけを追記または置換します。指定した場合、正常終了時に
`--cursor PATH` へ範囲限定のカーソルを書き込み、次回の実行で再利用できます。
`--kind rollout|state` を指定してください。`--max-polls COUNT` を指定すると
有限回の実行になり、省略すると停止されるまで `--interval-ms MILLISECONDS` ごとに
ポーリングします。

最終的な MVP 準備状況レビューと次フェーズへの移行条件は
[docs/readiness/mvp.md](docs/readiness/mvp.md) にあります。Phase 6 の回帰・安全性の
証拠は [docs/readiness/final-audit.md](docs/readiness/final-audit.md) に記録されています。
リリースノート、リリースチェックリスト、最小限のソースリリース手順は
[docs/release.md](docs/release.md)、現在のバージョン履歴は [CHANGELOG.md](CHANGELOG.md)
にあります。

範囲を限定し、人がレビューする検出結果評価計画は
[docs/evaluations/finding-usefulness-pilot.md](docs/evaluations/finding-usefulness-pilot.md)
にあります。これは計画用のワークシートです。実データ履歴の利用には所有者の
明示的な許可が必要で、`optimize --apply` はこの評価の対象外です。

実データ履歴を使う集計のみのスモーク手順は
[`scripts/real_history_smoke.py`](scripts/real_history_smoke.py) です。
手順は [CLI 仕様](docs/specs/cli.md#real-history-smoke-procedure) に記載されています。
選択した入力、ストア、レポート、コマンド出力はすべてこのリポジトリの外に
置いてください。実行スクリプトが記録するのは、範囲を限定した件数、カバレッジと
制約のサマリー、ストア鮮度、処理時間、元入力の不変性だけです。

以下のコマンド例は既存の派生ストアを使用し、読み取り専用の `--frozen` 境界を
指定します。

```bash
cargo run -- analyze --store .codexlens.sqlite --frozen
cargo run -- sessions --store .codexlens.sqlite --frozen
cargo run -- inventory --store .codexlens.sqlite --frozen
cargo run -- overhead --store .codexlens.sqlite --frozen
cargo run -- usage --store .codexlens.sqlite --frozen
cargo run -- waste --store .codexlens.sqlite --frozen
cargo run -- failures --store .codexlens.sqlite --frozen
cargo run -- stuck --store .codexlens.sqlite --frozen
cargo run -- prompts --store .codexlens.sqlite --frozen
cargo run -- corrections --store .codexlens.sqlite --frozen
cargo run -- rework --store .codexlens.sqlite --frozen
cargo run -- verification --store .codexlens.sqlite --frozen
cargo run -- knowledge --store .codexlens.sqlite --frozen
cargo run -- instructions --store .codexlens.sqlite --frozen
cargo run -- doctor --store .codexlens.sqlite --frozen --since 2026-01-01T00:00:00Z --until 2026-01-08T00:00:00Z
cargo run -- optimize --print --store .codexlens.sqlite --frozen
cargo run -- sql --store .codexlens.sqlite --format json SELECT/**/1
cargo run -- optimize --diff --store .codexlens.sqlite --frozen
cargo run -- monitor --source tests/fixtures/rollout/monitoring.jsonl --kind rollout --store .codexlens.sqlite --max-polls 1
cargo run -- doctor --format json --store .codexlens.sqlite --frozen
```

レポートコマンドは既定で選択した派生ストアを更新します。`--frozen` は
ストアのみを対象とする境界を
明示し、元入力を検出せず、ストアも書き込みません。`Activity` は選択した
ストアで観測された有効なタイムスタンプの最古・最新値です。`Latest ingestion`
はストアが入力を記録した別の時刻です。空、欠落、無効、不完全なタイムスタンプ
カバレッジは、取り込み時刻で補わずにそのまま報告します。

人が読む形式のメタデータ出力例:

```text
Coverage: selected store (observed; not necessarily all historical activity or current raw inputs; refresh explicitly, archives via --include-archived)
Activity: 2026-01-03T00:00:00.000Z .. 2026-01-04T00:05:00.000Z
Activity timestamps: 16 valid, 0 missing, 0 invalid
Sessions: 2
Records: 16
Latest ingestion: 2026-09-09T00:00:00Z
```

Phase 3 のレンズ、Phase 4 のアドバイザー、Phase 5 の安全な適用ワークフローは、
`codexlens::analysis` と `codexlens::advisor` モジュールから引き続き利用できます。
レンズは正規データを使い、ソースファイルを再度開きません。アドバイザーは
差分表示の際に推奨された指示ファイルだけを読み込みます。
[アーキテクチャ仕様](docs/specs/architecture.md)、
[セッション形式の契約](docs/specs/session-format.md)、
[分析契約](docs/specs/analysis.md) を参照してください。実装済みの Phase 5 境界の
互換性契約と、今後の拡張に必要な開始条件は
[docs/specs/post-mvp.md](docs/specs/post-mvp.md) に定義されています。

## 状況とロードマップ

Phase 0 から Phase 5 までの実装マイルストーンは完了しています。Phase 5 の
圧縮 rollout リーダー (#57)、refresh と frozen レポート (#58)、バージョン付き
JSON レポート (#59)、範囲を限定したローカルライブ監視 (#60)、安全な optimize
apply (#61) を実装済みです。今後の変更では、上記の明示的な境界を維持してください。

再ベースライン [#143](https://github.com/yuru-sha/codexlens/issues/143) は、
マージ済みの [#156](https://github.com/yuru-sha/codexlens/pull/156) により
クローズされました。現在の main に対するホスト CI マトリクスは成功しています。
製品の準備状況を確認するには、明示的に選択したローカルソースに対して、所有者の
許可を得た集計のみの実データ履歴スモーク実行が引き続き必要です。CI と
リポジトリ内のフィクスチャには、そのデータソースは含まれません。

Phase 6 はリリース準備フェーズです。リリースチェックリストとソースリリース
手順は [docs/release.md](docs/release.md) に記録されています。

- Phase 0 基盤: [#1](https://github.com/yuru-sha/codexlens/issues/1)–[#4](https://github.com/yuru-sha/codexlens/issues/4)
- Phase 1 Codex 取り込み: [#5](https://github.com/yuru-sha/codexlens/issues/5)–[#10](https://github.com/yuru-sha/codexlens/issues/10)
- Phase 2 指示: [#11](https://github.com/yuru-sha/codexlens/issues/11)–[#14](https://github.com/yuru-sha/codexlens/issues/14)
- Phase 3 レンズ: [#15](https://github.com/yuru-sha/codexlens/issues/15)–[#20](https://github.com/yuru-sha/codexlens/issues/20)
- Phase 4 アドバイザー: [#21](https://github.com/yuru-sha/codexlens/issues/21)–[#24](https://github.com/yuru-sha/codexlens/issues/24)
- Phase 5 圧縮 rollout リーダーのマイルストーン: [#57](https://github.com/yuru-sha/codexlens/issues/57)（実装済み）
- Phase 5 refresh と frozen レポート: [#58](https://github.com/yuru-sha/codexlens/issues/58)（実装済み）
- Phase 5 バージョン付き JSON レポート: [#59](https://github.com/yuru-sha/codexlens/issues/59)（実装済み）
- Phase 5 ローカルライブ監視: [#60](https://github.com/yuru-sha/codexlens/issues/60)（実装済み）
- Phase 5 安全な optimize apply: [#61](https://github.com/yuru-sha/codexlens/issues/61)（実装済み）

## 開発

必要条件: Rust 1.85 以降。

ローカル開発には `rust-toolchain.toml` で指定したツールチェーンを使用します。
`sh scripts/verify.sh` でローカル・CI 共通ゲートを実行できます。このゲートでは
プライバシーとステージ済み内容も検査します。権限や完了条件については
[環境構築手順](docs/agents/environment.md) と
[開発ワークフロー](docs/agents/workflow.md) を参照してください。Rust の各チェックは
次のとおりです。

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

GitHub Actions は Ubuntu で Rust 1.85.0 と 1.92.0 を使い、全ゲートを実行します。
固定された `macos-14` arm64 と `windows-latest` のジョブでは、プラットフォーム
ゲート（プライバシー検査、ビルド、全 feature のテスト）を実行します。macOS
ジョブではレポートのベンチマークも検査します。これらのジョブで使うのは
リポジトリ内の合成フィクスチャのみで、実際の Codex home は必要ありません。
Windows で保証する範囲はホスト runner 上のビルド・テストです。Windows の
実行時動作、パッケージ化、インストーラーはサポート対象としていません。

リポジトリのルールと合成フィクスチャの方針は [AGENTS.md](AGENTS.md) を参照してください。

## 着想

設計の参考にしたプロジェクト:

- [`cclens`](https://github.com/lambdalisue/cclens) — 設定された対象と観測した使用状況を
  関連付け、adapter と正規化ストアを分離する考え方。
- [`codex-session-insights`](https://github.com/cosformula/codex-session-insights)
  — Codex のローカル状態と rollout ファイルを実用的に検出する方法。

`codexlens` は独立した実装です。どちらのプロジェクトからもコードをコピーしていません。

## ライセンス

MIT。詳細は [LICENSE](LICENSE) を参照してください。

## GitHub Release

See docs/agents/release.md for the release note format and creation procedure. The shared body template is .github/release-notes-template.md, and the generated-note categories are managed in .github/release.yml.
