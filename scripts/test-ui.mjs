// 一键 UI 冒烟（docs/04 阶段 4）：起 dev server → 全部场景 + 1000 组大 fixture → 清理。
// 用法：npm run test:ui

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");
const BIG = resolve(ROOT, "scripts/fixtures/scan-result-big.json");
const SCENARIOS = ["results", "trash-confirm", "cancel", "trash-undo"];

const fail = (msg) => {
  console.error(`✗ ${msg}`);
  process.exit(1);
};

/** 等待 dev server 端口就绪 */
async function waitDevServer(url, tries = 30) {
  for (let i = 0; i < tries; i++) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
    } catch {
      /* 还没起来 */
    }
    await new Promise((r) => setTimeout(r, 1000));
  }
  fail("dev server 60s 内未就绪");
}

function run(cmd, args, env = {}) {
  return new Promise((ok, bad) => {
    const p = spawn(cmd, args, {
      cwd: ROOT,
      stdio: "inherit",
      env: { ...process.env, ...env },
    });
    p.on("exit", (c) => (c === 0 ? ok() : bad(new Error(`${cmd} ${args.join(" ")} 退出码 ${c}`))));
  });
}

// 大 fixture 不存在就先生成
if (!existsSync(BIG)) {
  console.log("[setup] 生成 1000 组大 fixture");
  await run("node", ["scripts/gen-big-fixture.mjs", "1000"]);
}

// 起 dev server（自己起自己杀，不依赖外部进程）
console.log("[setup] 启动 dev server");
const dev = spawn("npm", ["run", "dev"], { cwd: ROOT, stdio: "ignore" });
try {
  await waitDevServer("http://localhost:1420/");

  // 小 fixture 全场景
  for (const s of SCENARIOS) {
    console.log(`\n===== 场景：${s} =====`);
    await run("node", ["scripts/ui-smoke.mjs", s]);
  }

  // 1000 组大 fixture（DOM 规模 + 帧率门槛）
  console.log("\n===== 场景：1000 组大 fixture =====");
  await run("node", ["scripts/ui-smoke.mjs", "results"], { FIXTURE: "scripts/fixtures/scan-result-big.json" });

  console.log("\n✅ 全部 UI 冒烟通过");
} finally {
  dev.kill("SIGTERM");
}
