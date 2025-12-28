# sxiv-term: Terminal Image Viewer Project

SSH越しでsxivライクな操作感を実現するターミナル画像ビューワの開発計画。

## Background / 背景

### 課題
- MacからLinuxサーバーへSSH接続時、画像をサクサク見る手段がない
- X11フォワーディング（XQuartz）は遅い、Retina非対応
- 既存ツール（timg, viu等）はインタラクティブなブラウジングに非対応
- ranger/yazi等はファイルマネージャーであり、画像ビューワとしては過剰

### 目標
sxivのようにキーバインドでフォルダ内の画像を高速に横断できるツール

## Research / 調査結果

### ターミナル画像表示プロトコル

| プロトコル | 対応ターミナル | 品質 |
|-----------|---------------|------|
| Kitty Graphics | Kitty, WezTerm, Ghostty | 最高 |
| Sixel | xterm, mlterm, foot, mintty | 高 |
| iTerm2 | iTerm2, WezTerm | 高 |
| Unicode Halfblocks | 全て | 低（フォールバック） |

### 既存ツール比較

| ツール | 言語 | インタラクティブ | SSH対応 |
|--------|------|-----------------|---------|
| sxiv/nsxiv | C | ○ | △ (X11必須) |
| feh | C | ○ | △ (X11必須) |
| timg | C++ | × | ○ |
| viu | Rust | × | ○ |
| fim | C | ○ | △ (ASCII Art) |
| yazi | Rust | ○ (ファイルマネージャー) | ○ |

### 使用ライブラリ候補

#### Rust (推奨)
- **ratatui** (v0.28+): TUIフレームワーク
- **ratatui-image** (v8.1+): 画像表示ウィジェット
  - Kitty/Sixel/iTerm2/Halfblocks自動検出
  - GitHub: https://github.com/benjajaja/ratatui-image
- **crossterm**: ターミナル制御
- **image**: 画像読み込み・リサイズ

#### Go (代替)
- **bubbletea**: TUIフレームワーク
- **go-termimg**: 画像表示
  - GitHub: https://github.com/blacktop/go-termimg
- **rasterm**: シンプルな画像表示
  - GitHub: https://github.com/BourgeoisBear/rasterm

## Architecture / 設計

### ディレクトリ構成

```
sxiv-term/
├── Cargo.toml
├── src/
│   ├── main.rs          # エントリポイント
│   ├── app.rs           # アプリケーション状態管理
│   ├── viewer.rs        # 画像表示ロジック
│   ├── navigator.rs     # ファイル一覧・ナビゲーション
│   ├── keybinds.rs      # キーバインド処理
│   └── protocol.rs      # グラフィックスプロトコル検出
└── tests/
```

### 状態管理

```rust
struct App {
    images: Vec<PathBuf>,      // 画像ファイル一覧
    current_index: usize,      // 現在表示中のインデックス
    mode: ViewMode,            // Single / Thumbnail
    picker: Picker,            // ratatui-image プロトコル検出
}

enum ViewMode {
    Single,      // 1枚表示
    Thumbnail,   // グリッド表示
}
```

### キーバインド (sxiv互換)

| Key | Action |
|-----|--------|
| `j` / `Space` | 次の画像 |
| `k` / `Backspace` | 前の画像 |
| `g` | 最初の画像 |
| `G` | 最後の画像 |
| `t` | サムネイルモード切替 |
| `f` | フルスクリーン切替 |
| `r` | 画像リロード |
| `q` | 終了 |
| `Enter` | サムネイルから単一表示へ |
| `0-9` | 数値プレフィックス (5j = 5枚進む) |

## Implementation Plan / 実装計画

### Phase 1: MVP
1. [ ] プロジェクト初期化 (`cargo new sxiv-term`)
2. [ ] ratatui + ratatui-image セットアップ
3. [ ] 単一画像表示
4. [ ] j/k で前後移動
5. [ ] q で終了

### Phase 2: 基本機能
1. [ ] ディレクトリ内の画像自動収集
2. [ ] g/G で先頭/末尾ジャンプ
3. [ ] ステータスバー (ファイル名, インデックス)
4. [ ] 数値プレフィックス対応

### Phase 3: サムネイルモード
1. [ ] グリッド表示
2. [ ] サムネイル間のナビゲーション
3. [ ] Enter で単一表示へ

### Phase 4: 改善
1. [ ] 画像キャッシュ（先読み）
2. [ ] 設定ファイル対応
3. [ ] エラーハンドリング強化

## Dependencies / 依存関係

```toml
[package]
name = "sxiv-term"
version = "0.1.0"
edition = "2021"

[dependencies]
ratatui = "0.28"
ratatui-image = "8.1"
crossterm = "0.28"
image = "0.25"
anyhow = "1.0"
```

## References / 参考リンク

- [ratatui-image](https://github.com/benjajaja/ratatui-image)
- [ratatui](https://ratatui.rs/)
- [Kitty Graphics Protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/)
- [Are We Sixel Yet?](https://www.arewesixelyet.com/)
- [nsxiv (sxiv fork)](https://github.com/nsxiv/nsxiv)
- [timg](https://github.com/hzeller/timg)
- [yazi](https://github.com/sxyazi/yazi)

## Notes / メモ

- tmux内ではKittyプロトコルが制限される（Unicode placeholder使用で回避可能、tmux 3.3+）
- Sixelは256色パレット制限あり
- 先にKittyターミナルでの動作確認を推奨
