# 2025-12-26: Image Display Bug (Missing / Mismatch)

## 日付 / Date
- 2025-12-26

## 概要 / Summary
- ディレクトリ指定で起動後、キー長押しで高速移動するとまれに画像が表示されない、または別画像が表示される問題が発生。
- ステータスの●が緑 (Ready) でも画像が空のケースがあり、`r` でリロードすると表示される。

## 症状 / Symptoms
- Ready 表示だが画像が表示されない (空白)。
- 表示領域だけ合っていて、別画像がその領域に出る。

## 再現条件 / Steps
1. 画像ディレクトリを指定して起動。
2. `j` / `k` を長押しして高速移動。
3. まれに Ready 表示なのに画像が空白、または別画像が出る。
4. `r` でリロードすると表示される。

## 調査 / Investigation
- `SIVIT_TRACE_WORKER=1` で `/tmp/sivit_worker.log` を確認。
- 問題発生時でも該当ファイルは `decode/resize/encode` が完了している記録があり、ワーカー側は正常。

## 原因 / Root Cause (初期分析)
1) **KGP プレースホルダ色の衝突**
   - 画像 ID を連番で割り当てていたため、RGB が極端に低い値 (例: 0x010101) になりやすい。
   - 端末側の色量子化で ID が衝突し、別画像が表示される現象が発生。
2) **端末側キャッシュのエビクション**
   - 画像データは端末側に保持される前提で `ImagePlace` を行う。
   - 高速移動時に端末側が古い画像を破棄してしまうと、place-only では表示されず空白になる。
   - `r` で再送すると復旧することから、この挙動が濃厚。

## 対応履歴 / Fix Attempts

### Step 1-6: 初期対応 (成功)
- **epoch 導入**: 連続ナビゲーション時の古い描画結果を無視
- **KGP ID 分散化**: 24bit 空間で ID を分散し、RGB 各成分が一定以上になるよう割り当て
- **再送フェイルセーフ**: 一定数以上の送信を経た古いキャッシュは再送
- **結果**: 別画像表示の問題は解消

### Step 7: cancel 時に delete_by_id
- **変更**: CancelImage で in_flight_transmit の kgp_id を delete_by_id で削除
- **目的**: 途中まで送信された不完全な画像データをクリア
- **結果**: 部分的に改善

### Step 8: force_retransmit フラグ
- **変更**: cancel 時に force_retransmit = true を設定
- **目的**: 次の描画で ImagePlace ではなく ImageTransmit を強制
- **結果**: 1枚移動・リロードは改善、高速ナビは未解決

### Step 9: force_retransmit で新 kgp_id (ロールバック)
- **変更**: force_retransmit 時に新しい kgp_id を割り当て
- **問題**: encoded_chunks に古い kgp_id が埋め込まれているため、全画像が空白に
- **結果**: ロールバック

### Step 10: force_retransmit 時にキャッシュ削除
- **変更**: force_retransmit 時に現在画像のキャッシュを削除
- **結果**: 1枚移動・リロードは改善、高速ナビは未解決

### Step 11: pending_request クリア追加
- **変更**: force_retransmit 時に pending_request = None を追加
- **結果**: 効果なし

### Step 12: erase_and_place_rows (行単位 atomic)
- **変更**: erase と place を行単位で1チャンクにまとめる
- **目的**: cancel されても erase だけ適用されて place が適用されない問題を回避
- **問題**: 前の画像が残って新しい画像の領域だけが空白になる
- **分析**: delete_by_id が place した後に画像データを削除するため
- **結果**: 逆効果

### Step 13: erase 削除 + delete_by_id 遅延
- **変更**:
  - erase_rows を削除（place_rows のみ使用）
  - CancelImage で delete_by_id を即座に送信せず、pending_delete に保存
  - 次の ImageTransmit の最後で delete_by_id を送信
- **目的**: cancel されても空白にならないようにする
- **結果**: 前の画像が残って見にくい、空白になることもまだある

## 問題の詳細分析

### 高速ナビゲーション時の時系列
```
1. 画像 A が正常表示
2. キー長押し開始
3. 150ms 後、latch 解除 → ImageTransmit 開始
4. encoded_chunks 途中まで送信
5. erase/place チャンク途中まで送信
6. 次のキーリピート → CancelImage
   ├── current_task = None (残りチャンク破棄)
   └── delete_by_id で途中まで place した画像データ削除
7. 途中まで処理された行は空白に
8. これを繰り返すと全行が空白になる
```

### 根本的な問題
1. **チャンク送信とキャンセルのレース条件**: ImageTransmit のチャンク送信中に CancelImage が来ると、途中で中断される
2. **delete_by_id のタイミング**: 即座に送信すると place した画像が消える、遅延すると古いデータが残る
3. **erase と place の非原子性**: 全体として atomic ではないため、途中で中断されると不整合が発生

## 最終解決策 / Final Solution

### Step 14: 送信中はキャンセルしない (解決)

**変更内容**:
- `app.rs` に `is_transmitting()` メソッドを追加
- `main.rs` のナビゲーション処理で、送信中は `cancel_image_output()` をスキップ

```rust
// src/app.rs
pub fn is_transmitting(&self) -> bool {
    self.in_flight_transmit
}

// src/main.rs
if did_nav {
    if !app.is_transmitting() {
        app.cancel_image_output();
    }
    nav_until = Instant::now() + nav_latch;
    count = 0;
    break;
}
```

**結果**: 高速ナビゲーション後も画像が正しく表示されるようになった。

### なぜこれで解決したか

問題の本質は「送信途中でキャンセルされると、端末側に完全な画像データが残らない」ことだった。

1. `delete_by_id` で端末キャッシュを削除
2. `encoded_chunks` を送信開始
3. キー入力 → `cancel_image_output()` が呼ばれる
4. 送信が中断され、端末には不完全なデータのみ
5. 次の表示時に place しても画像がない → 空白

送信中はキャンセルをスキップすることで、送信が必ず完了し、端末側に有効な画像データが残る。

## Yazi からの学び / Lessons from Yazi

Yazi (Rust 製ターミナルファイルマネージャー) の KGP 実装を参考にした。

### 1. シングル ID 方式

Yazi はプロセスごとに固定の KGP ID を使用する。

```rust
// Yazi: adapter/kgp.rs
fn id() -> u32 {
    static ID: OnceLock<u32> = OnceLock::new();
    *ID.get_or_init(|| {
        let mut id = std::process::id();
        loop {
            let (r, g, b) = Self::split_id(id);
            if r >= 50 && g >= 50 && b >= 50 {
                return id;
            }
            id = id.wrapping_add(1);
        }
    })
}
```

**メリット**:
- 画像ごとに ID を変えると、端末キャッシュに古い画像が残り「別画像が表示される」問題が発生
- 固定 ID なら常に同じ場所を上書きするため、キャッシュ汚染がない

**sivit での適用**: `generate_kgp_id()` でプロセス ID ベースの固定 ID を生成。

### 2. hide → show パターン (Erase-First)

Yazi は描画前に必ず古い領域をクリアする。

```rust
// Yazi: adapter/kgp.rs
pub fn image_hide(self) -> Result<()> {
    let area = *self.area();
    Ueberzug::place(self)?;  // Clear the area
    Ok(self.term.move_lock((area.x, area.y), |_| {}))
}

pub fn image_show(self, url: &Url, ...) -> Result<()> {
    self.image_hide()?;  // Always hide first
    // Then show new image
}
```

**メリット**:
- 新しい画像を表示する前に古い画像を消すことで、表示の一貫性を保つ
- サイズが異なる画像間の遷移でゴーストが残らない

**sivit での適用**: `task_transmit` で erase → delete_by_id → transmit → place の順序を採用。

### 3. Atomic な操作の重要性

Yazi は画像操作を可能な限り atomic に行う。途中で中断されても不整合が起きにくい設計。

**sivit での教訓**:
- チャンク送信の途中でキャンセルすると不整合が発生
- キャンセルのタイミングを「送信中でないとき」に限定することで解決

## 影響範囲 / Affected Files

- `src/app.rs` - `is_transmitting()` 追加、シングル ID 方式
- `src/sender.rs` - erase-first パターン、delete_by_id 追加
- `src/kgp.rs` - `delete_by_id()` 関数追加
- `src/main.rs` - 送信中キャンセルスキップ

## ステータス / Status

**解決済み** (2025-12-26)
