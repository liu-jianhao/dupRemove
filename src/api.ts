import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { convertFileSrc } from "@tauri-apps/api/core";
import type {
  RestoreOutcome,
  ScanProgress,
  ScanResult,
  ThresholdLevel,
  TrashOutcome,
} from "./types";

/** 选择文件夹（原生对话框） */
export async function pickFolder(): Promise<string | null> {
  const picked = await open({
    directory: true,
    multiple: false,
    title: "选择要扫描的照片文件夹",
  });
  return typeof picked === "string" ? picked : null;
}

/** 扫描文件夹。档位是后端枚举（strict/balanced/loose），阈值数值只存在于后端。 */
export function startScan(
  dir: string,
  includeSubdirs: boolean,
  level: ThresholdLevel,
): Promise<ScanResult> {
  return invoke<ScanResult>("start_scan", {
    dir,
    includeSubdirs,
    level,
  });
}

/** 请求当前扫描协作式取消；返回是否真的取消了运行中的任务 */
export function cancelScan(): Promise<boolean> {
  return invoke<boolean>("cancel_scan");
}

/** 切换相似度档位后重聚类（指纹已缓存，秒级） */
export function regroup(level: ThresholdLevel): Promise<ScanResult> {
  return invoke<ScanResult>("regroup", { level });
}

/**
 * 移入系统废纸篓。
 *
 * 返回逐条结果 + 撤销批次 ID + 删除后重新分组的快照（保留项可能变化、
 * 只剩 1 张的组会消失），前端应把整份 groups/stats 替换掉。
 * 后端会校验每个路径属于当前扫描会话且文件身份未变，越界/被替换的会被拒绝。
 */
export function trashFiles(
  paths: string[],
  level: ThresholdLevel,
): Promise<TrashOutcome> {
  return invoke<TrashOutcome>("trash_files", { paths, level });
}

/**
 * 从废纸篓恢复（应用内撤销）。
 *
 * 只传 `trashFiles` 返回的批次 ID —— 恢复路径由后端批次注册表持有，
 * 前端伪造不了「从哪搬回哪」。
 */
export function restoreFiles(
  batchId: number,
  level: ThresholdLevel,
): Promise<RestoreOutcome> {
  return invoke<RestoreOutcome>("restore_files", { batchId, level });
}

/** 监听扫描进度 */
export function onScanProgress(
  handler: (p: ScanProgress) => void,
): Promise<() => void> {
  return listen<ScanProgress>("scan:progress", (e) => handler(e.payload));
}

/** 缩略图本地路径 → 可加载 URL（asset 协议，绝不经 IPC 传 base64） */
export function thumbUrl(thumbPath: string): string {
  if (!thumbPath) return "";
  return convertFileSrc(thumbPath);
}

export function formatBytes(bytes: number): string {
  if (bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(
    units.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1024)),
  );
  const value = bytes / Math.pow(1024, i);
  return `${value.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
