/**
 * 生成 README 用截图 / 演示 GIF（Playwright + 真实 UI + 夹具数据，非 Tauri 原生窗口）。
 * 用法：npm run capture:assets
 */
import { spawn } from "node:child_process";
import { mkdirSync, existsSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = fileURLToPath(new URL(".", import.meta.url));
const ROOT = resolve(__dirname, "..");
const ASSETS = resolve(ROOT, "docs/assets");

function run(cmd, args, env = {}) {
  return new Promise((ok, bad) => {
    const p = spawn(cmd, args, {
      cwd: ROOT,
      stdio: "inherit",
      env: { ...process.env, MARKETING_DIR: ASSETS, ...env },
    });
    p.on("exit", (c) => (c === 0 ? ok() : bad(new Error(`${cmd} 退出码 ${c}`))));
  });
}

async function waitDevServer(url, tries = 40) {
  for (let i = 0; i < tries; i++) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
    } catch {
      /* wait */
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("dev server 未就绪");
}

async function gifFromPngs(pngs, outGif) {
  const { spawnSync } = await import("node:child_process");
  const scale = "scale=920:-1:flags=lanczos";
  const dur = ["1.8", "2.2", "1.6"];
  const filters = pngs
    .map(
      (_, i) =>
        `[${i}:v]${scale},format=rgb24,trim=duration=${dur[i] ?? 1.5},setpts=PTS-STARTPTS[v${i}]`,
    )
    .join(";");
  const n = pngs.length;
  const concatIn = pngs.map((_, i) => `[v${i}]`).join("");
  const filterComplex = `${filters};${concatIn}concat=n=${n}:v=1:a=0[vcat];[vcat]split[s0][s1];[s0]palettegen=stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=3[out]`;
  const args = [
    "-y",
    ...pngs.flatMap((p) => ["-loop", "1", "-i", p]),
    "-filter_complex",
    filterComplex,
    "-map",
    "[out]",
    "-gifflags",
    "+transdiff",
    outGif,
  ];
  const r = spawnSync("ffmpeg", args, { stdio: "inherit" });
  if (r.status !== 0) throw new Error("ffmpeg 生成 GIF 失败");
}

mkdirSync(ASSETS, { recursive: true });

console.log("[capture] 启动 dev server");
const dev = spawn("npm", ["run", "dev"], { cwd: ROOT, stdio: "ignore" });
try {
  await waitDevServer("http://localhost:1420/");

  for (const scenario of ["empty", "cancel", "results", "trash-confirm"]) {
    console.log(`\n[capture] 场景 ${scenario}`);
    await run("node", ["scripts/ui-smoke.mjs", scenario]);
  }

  const hero = resolve(ASSETS, "results.png");
  const lightbox = resolve(ASSETS, "lightbox.png");
  const empty = resolve(ASSETS, "empty.png");
  for (const p of [hero, lightbox, empty]) {
    if (!existsSync(p)) throw new Error(`缺少截图 ${p}`);
  }

  console.log("\n[capture] 合成 demo.gif");
  await gifFromPngs([empty, hero, lightbox], resolve(ASSETS, "demo.gif"));

  console.log("\n✅ 素材已写入 docs/assets/");
} finally {
  dev.kill("SIGTERM");
}
