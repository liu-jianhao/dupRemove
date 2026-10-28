// 生成 1000 组大结果集 fixture（docs/04 阶段 3：大结果集 DOM 实测）。
// 用法：node scripts/gen-big-fixture.mjs [组数]（默认 1000）
//
// 基于真实扫描 fixture 复制放大：path 加唯一后缀（thumb_path 保持复用真缩略图），
// stats 按新组数重算。

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, "..");
const N = Number(process.argv[2] ?? 1000);

const src = JSON.parse(
  readFileSync(resolve(ROOT, "scripts/fixtures/scan-result.json"), "utf8"),
);

const groups = [];
for (let gi = 0; groups.length < N; gi++) {
  for (const g of src.groups) {
    if (groups.length >= N) break;
    groups.push({
      kind: g.kind,
      files: g.files.map((f, k) => ({ ...f, path: `${f.path}#g${gi}f${k}` })),
    });
  }
}

const stats = {
  ...src.stats,
  exact_groups: groups.filter((g) => g.kind === "exact").length,
  similar_groups: groups.filter((g) => g.kind === "similar").length,
  scanned: groups.reduce((s, g) => s + g.files.length, 0),
};

const out = resolve(ROOT, "scripts/fixtures/scan-result-big.json");
writeFileSync(out, JSON.stringify({ groups, stats }));
console.log(`生成 ${groups.length} 组（${stats.scanned} 张）→ ${out}`);
