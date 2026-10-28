import { useEffect, useRef, useState } from "react";
import "./App.css";
import { cancelScan, onScanProgress, regroup, startScan } from "./api";
import { EmptyState } from "./components/EmptyState";
import { Results } from "./components/Results";
import { Scanning } from "./components/Scanning";
import type { ScanProgress, ScanResult, ThresholdLevel } from "./types";

type Stage = "empty" | "scanning" | "results";

export default function App() {
  const [stage, setStage] = useState<Stage>("empty");
  const [result, setResult] = useState<ScanResult | null>(null);
  const [progress, setProgress] = useState<ScanProgress | null>(null);
  const [folder, setFolder] = useState<string | null>(null);
  const [level, setLevel] = useState<ThresholdLevel>("balanced");
  const [busy, setBusy] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // 操作序号：regroup / 扫描的旧响应不得覆盖用户已重置或已切换档位后的新状态。
  // 每次发起操作 +1，响应回来时序号不匹配就丢弃。
  const opSeq = useRef(0);

  // 订阅扫描进度
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let alive = true;
    onScanProgress((p) => {
      if (alive) setProgress(p);
    }).then((fn) => {
      if (alive) unlisten = fn;
      else fn();
    });
    return () => {
      alive = false;
      unlisten?.();
    };
  }, []);

  const handleScan = async (
    dir: string,
    includeSubdirs: boolean,
    scanLevel: ThresholdLevel,
  ) => {
    const seq = ++opSeq.current;
    setError(null);
    setCancelling(false);
    setFolder(dir);
    setProgress({ stage: "scanning", done: 0, total: 0 });
    setStage("scanning");
    try {
      const r = await startScan(dir, includeSubdirs, scanLevel);
      if (seq !== opSeq.current) return; // 过期响应：期间状态已被重置
      setResult(r);
      setStage("results");
    } catch (e) {
      if (seq !== opSeq.current) return;
      setError(String(e));
      setStage("empty");
    } finally {
      if (seq === opSeq.current) setCancelling(false);
    }
  };

  const handleCancel = async () => {
    if (cancelling) return;
    setCancelling(true);
    setError(null);
    try {
      await cancelScan();
    } catch (e) {
      setError(String(e));
      setCancelling(false);
    }
  };

  const handleLevelChange = async (next: ThresholdLevel) => {
    if (next === level || !result) return;
    const seq = ++opSeq.current;
    setBusy(true);
    try {
      const r = await regroup(next);
      if (seq !== opSeq.current) return; // 过期响应：避免旧档位的分组覆盖新状态
      setLevel(next);
      setResult(r);
    } catch (e) {
      if (seq !== opSeq.current) return;
      setError(String(e));
    } finally {
      if (seq === opSeq.current) setBusy(false);
    }
  };

  const rescan = () => {
    ++opSeq.current; // 使所有在途响应失效
    setResult(null);
    setProgress(null);
    setFolder(null);
    setError(null);
    setCancelling(false);
    setStage("empty");
  };

  return (
    <div className="app">
      {stage === "empty" && (
        <EmptyState
          onScan={handleScan}
          error={error}
          defaultLevel={level}
          onLevelChange={setLevel}
        />
      )}
      {stage === "scanning" && (
        <Scanning
          progress={progress}
          folder={folder}
          cancelling={cancelling}
          error={error}
          onCancel={handleCancel}
        />
      )}
      {stage === "results" && result && (
        <Results
          result={result}
          level={level}
          busy={busy}
          onLevelChange={handleLevelChange}
          onRescan={rescan}
          onResultReplaced={setResult}
        />
      )}
    </div>
  );
}
