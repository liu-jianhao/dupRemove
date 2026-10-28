import { useEffect, useMemo, useRef, useState } from "react";
import { formatBytes, restoreFiles, thumbUrl, trashFiles } from "../api";
import type {
  ScanResult,
  ThresholdLevel,
} from "../types";
import { THRESHOLD_LABELS, THRESHOLDS } from "../types";

interface Props {
  result: ScanResult;
  level: ThresholdLevel;
  busy: boolean;
  onLevelChange: (level: ThresholdLevel) => void;
  onRescan: () => void;
  /**
   * 删除/撤销后由父组件把整份 result 换掉。
   *
   * 为什么不在本组件里就地删几项：删除会改变「保留项」（删掉建议保留的那张，
   * 另一张要顶上），也会让只剩 1 张的组整组消失。这些都必须由后端重新聚类得出，
   * 前端自己算容易和后端结果漂移。
   */
  onResultReplaced: (next: ScanResult) => void;
}

/** 撤销窗口的秒数。给用户看清楚发生了什么，又不至于让「已删除」的状态长期悬着。 */
const UNDO_WINDOW_SECONDS = 8;

interface PendingUndo {
  /** 后端签发的撤销批次 ID —— 恢复路径只在后端，前端伪造不了 */
  batchId: number;
  count: number;
  bytes: number;
  /** 剩余秒数（0 表示窗口已关闭） */
  remaining: number;
}

export function Results({
  result,
  level,
  busy,
  onLevelChange,
  onRescan,
  onResultReplaced,
}: Props) {
  const { groups, stats } = result;

  // 勾选集合：待移入废纸篓的文件路径。
  // 默认勾选每组的"非最优项"；最优项（系统建议保留的那张）默认不勾，
  // 但用户可以随时改 —— 包括把它也勾上，整组一起清掉。
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirming, setConfirming] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const [trashing, setTrashing] = useState(false);
  const [restoring, setRestoring] = useState(false);
  const [pendingUndo, setPendingUndo] = useState<PendingUndo | null>(null);
  const undoTimerRef = useRef<number | null>(null);
  // 大图对比：当前预览的 (组下标, 文件下标)
  const [preview, setPreview] = useState<{ gi: number; fi: number } | null>(null);

  // lightbox 键盘：Escape 关闭，← → 在组内切换
  useEffect(() => {
    if (!preview) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setPreview(null);
      if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
        setPreview((p) => {
          if (!p) return p;
          const files = groups[p.gi].files;
          const next = p.fi + (e.key === "ArrowRight" ? 1 : -1);
          return next >= 0 && next < files.length ? { ...p, fi: next } : p;
        });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [preview, groups]);

  // 分组变化时重置勾选。
  // 决策 #2（2026-10-28）：只有「内容一致」的项默认勾选 —— 完全相同的组
  // （组徽章已说明）+ 相似组里 identical=true 的真副本（删了零损失）。
  // 「仅相似」的照片必须用户逐张手动勾选：误聚时默认删除的风险太高。
  useEffect(() => {
    const next = new Set<string>();
    for (const g of groups) {
      for (const f of g.files) {
        if (!f.is_best && (g.kind === "exact" || f.identical)) {
          next.add(f.path);
        }
      }
    }
    setSelected(next);
  }, [groups]);

  // 撤销窗口倒计时：每秒减 1，归零后自动关闭。
  useEffect(() => {
    if (!pendingUndo) return;
    if (pendingUndo.remaining <= 0) {
      setPendingUndo(null);
      return;
    }
    undoTimerRef.current = window.setTimeout(() => {
      setPendingUndo((prev) =>
        prev ? { ...prev, remaining: prev.remaining - 1 } : prev,
      );
    }, 1000);
    return () => {
      if (undoTimerRef.current != null) {
        window.clearTimeout(undoTimerRef.current);
        undoTimerRef.current = null;
      }
    };
  }, [pendingUndo]);

  const toggle = (path: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  /** 整组批量勾选 / 取消 —— 用户经常想「这组全删」或「这组都留着」 */
  const setGroup = (paths: string[], on: boolean) => {
    setSelected((prev) => {
      const next = new Set(prev);
      for (const p of paths) {
        if (on) next.add(p);
        else next.delete(p);
      }
      return next;
    });
  };

  const selectedBytes = useMemo(() => {
    let total = 0;
    for (const g of groups) {
      for (const f of g.files) {
        if (selected.has(f.path)) total += f.size;
      }
    }
    return total;
  }, [groups, selected]);

  const doTrash = async () => {
    const paths = [...selected];
    // 提前算好字节数：删除后 groups 已经变了，selected 里的路径也拿不到 size 了。
    const bytes = selectedBytes;
    setTrashing(true);
    try {
      const outcome = await trashFiles(paths, level);
      const moved = outcome.items.filter((i) => i.trash_path != null);
      const failed = outcome.items.filter((i) => i.trash_path == null);

      // 后端已经重新聚好组，整份替换 —— 保留项/组数/统计都是自洽的。
      onResultReplaced({ groups: outcome.groups, stats: outcome.stats });
      setConfirming(false);
      setSelected(new Set());

      if (moved.length > 0 && outcome.batch_id != null) {
        setPendingUndo({
          batchId: outcome.batch_id,
          count: moved.length,
          bytes,
          remaining: UNDO_WINDOW_SECONDS,
        });
      }
      if (failed.length > 0) {
        const first = failed[0].error ?? "未知原因";
        setToast(
          `${failed.length} 张未能移入废纸篓（${first}）${
            moved.length > 0 ? `，另 ${moved.length} 张已成功` : ""
          }`,
        );
      } else {
        setToast(null);
      }
    } catch (e) {
      setToast(`移入废纸篓失败：${e}`);
    } finally {
      setTrashing(false);
    }
  };

  const doUndo = async () => {
    if (!pendingUndo) return;
    const snapshot = pendingUndo;
    setRestoring(true);
    // 立刻关掉倒计时条：避免用户重复点击，也避免恢复过程中窗口自然过期。
    setPendingUndo(null);
    try {
      const outcome = await restoreFiles(snapshot.batchId, level);
      onResultReplaced({ groups: outcome.groups, stats: outcome.stats });
      const failed = outcome.items.filter(
        (i) => i.trash_path != null && i.error != null,
      );
      if (failed.length > 0) {
        const first = failed[0].error ?? "未知原因";
        setToast(
          `${failed.length} 张撤销失败（${first}），其余已回到列表`,
        );
      } else {
        setToast(`已撤销，${snapshot.count} 张照片回到列表`);
      }
    } catch (e) {
      setToast(`撤销失败：${e}`);
    } finally {
      setRestoring(false);
    }
  };

  if (groups.length === 0 && !pendingUndo) {
    return (
      <div className="results">
        <div className="results-head">
          <h2>没有发现重复照片</h2>
          <p className="muted">
            扫描了 {stats.scanned} 张图片（缓存命中 {stats.cache_hits} 张），没有找到重复或相似的照片。
          </p>
          <button className="ghost" onClick={onRescan}>
            换一个文件夹
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="results">
      <div className="results-head">
        <div className="head-left">
          <h2>
            发现 <span className="num">{groups.length}</span> 组重复
          </h2>
          <p className="muted">
            完全相同 {stats.exact_groups} 组 · 几乎一样 {stats.similar_groups} 组 ·
            扫描 {stats.scanned} 张（缓存命中 {stats.cache_hits}）
            {stats.skipped_placeholder > 0 &&
              ` · 跳过 ${stats.skipped_placeholder} 个云端未下载文件`}
            {stats.walk_errors > 0 && ` · ${stats.walk_errors} 个文件无法访问`}
          </p>
        </div>
        <div className="head-right">
          <div className="segmented small">
            {(Object.keys(THRESHOLDS) as ThresholdLevel[]).map((k) => (
              <button
                key={k}
                className={level === k ? "seg active" : "seg"}
                disabled={busy || trashing || restoring}
                onClick={() => onLevelChange(k)}
                title={`结构差异 ${THRESHOLDS[k].phash} 以内、颜色差异 ${THRESHOLDS[k].color} 以内才算「几乎一样」`}
              >
                {THRESHOLD_LABELS[k]}
              </button>
            ))}
          </div>
          <button
            className="ghost"
            disabled={busy || trashing || restoring}
            onClick={onRescan}
          >
            换文件夹
          </button>
        </div>
      </div>

      {groups.length === 0 ? (
        <div className="all-clean">
          <p>这一档下已经没有重复照片了。</p>
          <p className="muted">
            可以切换到更宽松的档位再找找，或者换一个文件夹。
          </p>
        </div>
      ) : (
        <>
          <p className="hint">
            勾上 = 移入废纸篓。系统默认只勾选「内容一致」的副本（删除零损失）；
            标着「仅相似」的照片内容其实不同，点开大图确认后再手动勾选。
            绿点是系统建议保留的那张。
          </p>

          <div className="group-list">
            {groups.map((g, gi) => {
              const paths = g.files.map((f) => f.path);
              const allOn = paths.every((p) => selected.has(p));
              const noneOn = paths.every((p) => !selected.has(p));
              return (
                <section className="group" key={gi}>
                  <div className="group-title">
                    <span className={g.kind === "exact" ? "badge exact" : "badge similar"}>
                      {g.kind === "exact" ? "完全相同" : "几乎一样"}
                    </span>
                    <span className="muted">
                      {g.files.length} 张 · 可释放{" "}
                      {formatBytes(
                        g.files.filter((f) => !f.is_best).reduce((s, f) => s + f.size, 0),
                      )}
                    </span>
                    <div className="group-actions">
                      <button
                        className="link"
                        disabled={allOn}
                        onClick={() => setGroup(paths, true)}
                        title="勾选这一组的全部照片"
                      >
                        全选
                      </button>
                      <button
                        className="link"
                        disabled={noneOn}
                        onClick={() => setGroup(paths, false)}
                        title="取消勾选这一组的全部照片"
                      >
                        全不选
                      </button>
                    </div>
                  </div>
                  <div className="thumbs">
                    {g.files.map((f, fi) => {
                      const isSelected = selected.has(f.path);
                      return (
                        <div
                          key={f.path}
                          className={isSelected ? "thumb marked" : "thumb"}
                          title={f.path}
                        >
                          <div className="thumb-img">
                            <img
                              src={thumbUrl(f.thumb_path)}
                              alt={f.path.split("/").pop() ?? ""}
                              loading="lazy"
                              onClick={() => setPreview({ gi, fi })}
                            />
                            {/* 勾选框：每一张都有，包括系统建议保留的那张 */}
                            <label
                              className={isSelected ? "pick on" : "pick"}
                              title={
                                isSelected
                                  ? "已标记：移入废纸篓。点击取消"
                                  : "未标记：保留。点击标记为移入废纸篓"
                              }
                            >
                              <input
                                type="checkbox"
                                checked={isSelected}
                                onChange={() => toggle(f.path)}
                                aria-label={
                                  isSelected
                                    ? `取消删除 ${f.path.split("/").pop()}`
                                    : `标记删除 ${f.path.split("/").pop()}`
                                }
                              />
                            </label>
                            {/* 只有「几乎一样」的组才需要逐张区分：
                                完全相同的组里每个成员都与保留项字节一致，组徽章已经说清楚了。
                                几乎一样的组里才是混着「真副本」和「只是重压缩/缩放版」，
                                这个区别直接决定删掉会不会掉画质，必须显式标出来。 */}
                            {!f.is_best && g.kind === "similar" && (
                              <span
                                className={
                                  f.identical ? "content-tag same" : "content-tag diff"
                                }
                                title={
                                  f.identical
                                    ? "与保留的那张内容完全一致，删掉不会有任何损失"
                                    : "只是看起来像同一张，实际内容不同，删除前建议先确认"
                                }
                              >
                                {f.identical ? "内容一致" : "仅相似"}
                              </span>
                            )}
                          </div>
                          <div className="thumb-meta">
                            {f.is_best && (
                              <i
                                className="keep-dot"
                                title="系统建议保留这一张（仍可勾选删掉）"
                              />
                            )}
                            {f.width}×{f.height} · {formatBytes(f.size)}
                          </div>
                        </div>
                      );
                    })}
                  </div>
                </section>
              );
            })}
          </div>

          <div className="action-bar">
            <div className="action-info">
              共 {groups.length} 组 · 选中 <span className="num">{selected.size}</span> 张 · 可释放{" "}
              <span className="num">{formatBytes(selectedBytes)}</span>
            </div>
            {/* 撤销窗口期间禁用新删除：连续删除会顶掉上一批的撤销入口，
                干脆等 8 秒窗口过去或先撤销，保证任何一批都可撤销到底。 */}
            <button
              className="danger"
              disabled={selected.size === 0 || trashing || pendingUndo != null}
              title={pendingUndo != null ? "撤销窗口期间暂不能删除，请先撤销或等待倒计时结束" : undefined}
              onClick={() => setConfirming(true)}
            >
              移入废纸篓
            </button>
          </div>
        </>
      )}

      {confirming && (
        <div className="modal-mask" onClick={() => setConfirming(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>确认移入废纸篓？</h3>
            <p>
              将把 <span className="num">{selected.size}</span> 张照片移入系统废纸篓，
              释放 <span className="num">{formatBytes(selectedBytes)}</span> 空间。
            </p>
            <p className="muted">不会被永久删除 —— 你随时可以从系统废纸篓里恢复。</p>
            <div className="modal-actions">
              <button className="ghost" onClick={() => setConfirming(false)}>
                取消
              </button>
              <button className="danger" onClick={doTrash} disabled={trashing}>
                {trashing ? "处理中…" : "确认移入"}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 大图对比（docs/04 阶段 3）：「仅相似」的照片 88px 缩略图不足以安全判断，
          点击放大 + 完整路径/尺寸信息。
          ponytail: 大图复用 320px 缩略图缓存（asset 协议只放行缩略图目录，
          放开原图读取会破坏最小权限）；需要更清晰的预览时升级为更大尺寸的
          缩略图缓存版本（THUMB_ALGO_VERSION bump），而非放开原图 scope。 */}
      {preview && groups[preview.gi] && (
        <div className="lightbox-mask" onClick={() => setPreview(null)}>
          <div className="lightbox" onClick={(e) => e.stopPropagation()}>
            {(() => {
              const g = groups[preview.gi];
              const f = g.files[preview.fi];
              const isSelected = selected.has(f.path);
              const role = f.is_best
                ? "建议保留"
                : f.identical
                  ? "与保留项内容一致"
                  : g.kind === "similar"
                    ? "仅相似（内容不同）"
                    : "";
              return (
                <>
                  <img
                    className="lightbox-img"
                    src={thumbUrl(f.thumb_path)}
                    alt={f.path.split("/").pop() ?? ""}
                  />
                  <div className="lightbox-meta">
                    <b>{f.path.split("/").pop()}</b> · {f.width}×{f.height} ·{" "}
                    {formatBytes(f.size)}
                    {role && ` · ${role}`}
                  </div>
                  <div className="lightbox-path" title={f.path}>
                    {f.path}
                  </div>
                  <div className="lightbox-actions">
                    <button
                      className={isSelected ? "danger" : "ghost"}
                      onClick={() => toggle(f.path)}
                    >
                      {isSelected ? "取消删除勾选" : "勾选移入废纸篓"}
                    </button>
                  </div>
                  <div className="lightbox-strip">
                    {g.files.map((s, k) => (
                      <img
                        key={s.path}
                        src={thumbUrl(s.thumb_path)}
                        className={k === preview.fi ? "on" : ""}
                        onClick={() => setPreview({ gi: preview.gi, fi: k })}
                        alt={s.path.split("/").pop() ?? ""}
                        title={s.path.split("/").pop()}
                      />
                    ))}
                  </div>
                  <p className="lightbox-hint">← → 切换 · Esc 关闭</p>
                </>
              );
            })()}
          </div>
        </div>
      )}

      {/* 常驻撤销条：删除后 8 秒内可见，倒计时结束自动消失。
          比一闪而过的 toast 更符合「不可逆操作要有后悔药」的直觉。 */}
      {pendingUndo && (
        <div className="undo-bar" role="status" aria-live="polite">
          <div className="undo-text">
            已把 <span className="num">{pendingUndo.count}</span> 张照片
            （<span className="num">{formatBytes(pendingUndo.bytes)}</span>）
            移入系统废纸篓
          </div>
          <div className="undo-actions">
            <span className="undo-countdown" aria-hidden="true">
              {pendingUndo.remaining}s
            </span>
            <button
              className="undo-btn"
              onClick={doUndo}
              disabled={restoring}
            >
              {restoring ? "撤销中…" : "撤销"}
            </button>
          </div>
          <div
            className="undo-progress"
            style={{
              width: `${(pendingUndo.remaining / UNDO_WINDOW_SECONDS) * 100}%`,
            }}
            aria-hidden="true"
          />
        </div>
      )}

      {toast && (
        <div className="toast" onClick={() => setToast(null)}>
          {toast}
        </div>
      )}
    </div>
  );
}
