import type { ScanProgress } from "../types";

interface Props {
  progress: ScanProgress | null;
  folder: string | null;
  cancelling: boolean;
  error: string | null;
  onCancel: () => void;
}

const STAGE_LABELS: Record<string, string> = {
  scanning: "正在遍历文件夹",
  cache: "正在读取缓存",
  fingerprints: "正在计算指纹",
  exact_hash: "正在校验精确重复",
  clustering: "正在聚类比对",
  splitting: "正在校验分组一致性",
  thumbnails: "正在生成缩略图",
};

export function Scanning({ progress, folder, cancelling, error, onCancel }: Props) {
  const stage = progress?.stage ?? "scanning";
  const total = progress?.total ?? 0;
  const done = progress?.done ?? 0;
  const pct = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 0;
  const stageLabel = STAGE_LABELS[stage] ?? stage;

  return (
    <div className="scanning">
      <div className="scan-card">
        <div className="spinner" aria-hidden="true" />
        <h2>{cancelling ? "正在取消…" : "正在扫描"}</h2>
        {folder && <p className="scan-folder">{folder}</p>}
        <p className="scan-stage">{stageLabel}</p>
        <div className="progress">
          <div className="progress-bar" style={{ width: `${pct}%` }} />
        </div>
        <p className="scan-count">
          {total > 0
            ? `${done} / ${total}`
            : stage === "scanning"
              ? `已发现 ${done} 张`
              : "准备中…"}
        </p>
        {error && <p className="scan-error">{error}</p>}
        {!cancelling && (
          <>
            <p className="scan-hint">首次扫描需要逐张解码并计算哈希，之后再扫会快很多。</p>
            <button className="cancel-btn" onClick={onCancel}>
              取消扫描
            </button>
          </>
        )}
      </div>
    </div>
  );
}
