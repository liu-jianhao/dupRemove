// 与 Rust 侧 pipeline.rs 的序列化结构一一对应

export interface FileJson {
  path: string;
  size: number;
  width: number;
  height: number;
  thumb_path: string;
  is_best: boolean;
  /**
   * 与本组保留项是同一份文件（内容逐字节一致）。
   * UI 用它在「几乎一样」的组里区分「内容一致」（删了零损失）与「仅相似」（删前需确认）。
   */
  identical: boolean;
}

export interface GroupJson {
  kind: "exact" | "similar";
  files: FileJson[];
}

export interface ScanStats {
  scanned: number;
  decoded: number;
  skipped_decode: number;
  /** 跳过的云占位文件数（iCloud/OneDrive 未下载，读取会触发联网，绝不打开） */
  skipped_placeholder: number;
  /** 遍历失败的条目数（权限不足等） */
  walk_errors: number;
  cache_hits: number;
  new_fingerprints: number;
  exact_groups: number;
  similar_groups: number;
  duplicate_files: number;
  reclaimable_bytes: number;
}

export interface ScanResult {
  groups: GroupJson[];
  stats: ScanStats;
}

export interface ScanProgress {
  stage: string;
  done: number;
  total: number;
}

/**
 * 单个文件的移入/恢复结果。
 *
 * `trash_path` 为 null 表示这一步失败（原因见 `error`）；
 * 撤销不再回传 items —— 前端只传后端签发的 batch_id。
 */
export interface TrashItem {
  original: string;
  trash_path: string | null;
  error: string | null;
}

/** `trash_files` 返回值：逐条结果 + 撤销批次 ID + 删除后重新分组的快照 */
export interface TrashOutcome {
  items: TrashItem[];
  /** 后端签发的撤销批次 ID（无成功项时为 null）。撤销时原样传回 restoreFiles。 */
  batch_id: number | null;
  groups: GroupJson[];
  stats: ScanStats;
}

/** `restore_files` 返回值：逐条结果 + 恢复后重新分组的快照 */
export interface RestoreOutcome {
  items: TrashItem[];
  groups: GroupJson[];
  stats: ScanStats;
}

export type ThresholdLevel = "strict" | "balanced" | "loose";

/**
 * 相似度档位 → 双闸门阈值。
 *
 * 「几乎一样」要同时通过两关：
 * - phash：结构哈希（pHash）汉明距离上限。越小越严。
 * - color：颜色签名平均差上限（0~255）。越小越严。
 *
 * 数值与后端 `cluster::THRESHOLDS` 一一对应，改动必须两边同步。
 *
 * 实测依据（19 张真实照片 + 3500+ 对组合）：
 * - 真重复（缩放 / 变暗 / 变亮 / 平移 / 重压）→ 结构 0~3、颜色 0~5
 * - 真重复（不同文件但内容相同）→ 结构 15、颜色 9
 * - 同构图换配色 → 结构 3~14、颜色 13~85
 * - 完全不同的内容 → 结构 30、颜色 89
 *
 * 颜色阈值要落在 9~13 之间（真重复上界与换配色下界之间），窗口很窄；
 * 结构阈值相应放宽到 16，让「结构相近但配色不同」由颜色通道兜住。
 */
export const THRESHOLDS: Record<
  ThresholdLevel,
  { phash: number; color: number }
> = {
  strict: { phash: 12, color: 8 },
  balanced: { phash: 16, color: 12 },
  loose: { phash: 22, color: 25 },
};

export const THRESHOLD_LABELS: Record<ThresholdLevel, string> = {
  strict: "严格",
  balanced: "均衡",
  loose: "宽松",
};
