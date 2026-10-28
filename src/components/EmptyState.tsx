import { useState } from "react";
import { pickFolder } from "../api";
import type { ThresholdLevel } from "../types";
import { THRESHOLD_LABELS, THRESHOLDS } from "../types";

interface Props {
  onScan: (dir: string, includeSubdirs: boolean, level: ThresholdLevel) => void;
  error: string | null;
  defaultLevel: ThresholdLevel;
  onLevelChange: (level: ThresholdLevel) => void;
}

export function EmptyState({ onScan, error, defaultLevel, onLevelChange }: Props) {
  const [includeSubdirs, setIncludeSubdirs] = useState(true);
  const [busy, setBusy] = useState(false);

  const handlePick = async () => {
    setBusy(true);
    try {
      const dir = await pickFolder();
      if (dir) onScan(dir, includeSubdirs, defaultLevel);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="empty">
      <div className="empty-hero">
        <div className="logo" aria-hidden="true">
          <span className="logo-card" />
          <span className="logo-card logo-card-dup" />
        </div>
        <h1>重复照片，扫干净</h1>
        <p className="promise">你的照片不会离开这台电脑 —— 全程离线，不联网，不上传。</p>
      </div>

      <div className="empty-panel">
        <div className="field">
          <span className="field-label">相似度</span>
          <div className="segmented">
            {(Object.keys(THRESHOLDS) as ThresholdLevel[]).map((k) => (
              <button
                key={k}
                className={defaultLevel === k ? "seg active" : "seg"}
                onClick={() => onLevelChange(k)}
                title={`识别严格程度：越严格越不容易误判，宽松则连拍、轻微裁切也会归为一组（结构差异 ${THRESHOLDS[k].phash} 以内、颜色差异 ${THRESHOLDS[k].color} 以内）`}
              >
                {THRESHOLD_LABELS[k]}
              </button>
            ))}
          </div>
          <span className="field-hint">
            严格 = 只认几乎一样；宽松 = 连拍、轻微裁切也会归为一组
          </span>
        </div>

        <label className="checkbox">
          <input
            type="checkbox"
            checked={includeSubdirs}
            onChange={(e) => setIncludeSubdirs(e.target.checked)}
          />
          包含子文件夹
        </label>

        <button className="primary" onClick={handlePick} disabled={busy}>
          {busy ? "选择中…" : "选择文件夹"}
        </button>

        {error && <div className="error">{error}</div>}

        <ul className="empty-notes">
          <li>完全相同的照片，逐字节比对确认是同一个文件</li>
          <li>
            几乎一样的照片，缩放、转存、轻压缩也能认出来 —— 同时比对画面结构与颜色
          </li>
          <li>删除 = 移入系统废纸篓，随时可以恢复</li>
        </ul>
      </div>
    </div>
  );
}
