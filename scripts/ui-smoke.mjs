/**
 * UI 冒烟：mock Tauri IPC 驱动前端三屏，验证结果页渲染与交互。
 *
 * 为什么需要 mock：Tauri 的 invoke 走原生 IPC，浏览器环境无实现。
 * 这里在页面加载前注入 window.__TAURI_INTERNALS__.invoke，
 * 用真实扫描产出的 JSON 作为返回值 —— 验证的是真实数据在真实组件里的表现。
 *
 * 用法：node scripts/ui-smoke.mjs [场景名]
 *   scenarios: empty | results | trash-confirm | cancel | trash-undo
 */
import { chromium } from "playwright";
import { readFileSync, mkdirSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { homedir } from "node:os";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(__dirname, "..");
const OUT = resolve(ROOT, ".shots");
const BASE = process.env.DEV_URL ?? "http://localhost:1420/";

const scenario = process.argv[2] ?? "results";

/** 复用本机已安装的 Chromium（避免重复下载） */
function findChromium() {
  const base = resolve(homedir(), "Library/Caches/ms-playwright");
  const candidates = [
    "chromium_headless_shell-1228/chrome-headless-shell-mac-arm64/chrome-headless-shell",
    "chromium-1228/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    "chromium-1217/chrome-mac/Chromium.app/Contents/MacOS/Chromium",
  ];
  for (const c of candidates) {
    const p = resolve(base, c);
    if (existsSync(p)) return p;
  }
  return undefined; // 回退到 playwright 自带
}

/** 读取真实扫描产出（由 dupremove-cli --json 生成）；FIXTURE 环境变量可换大结果集 */
function loadScanResult() {
  const p = process.env.FIXTURE
    ? resolve(process.env.FIXTURE)
    : resolve(ROOT, "scripts/fixtures/scan-result.json");
  return JSON.parse(readFileSync(p, "utf8"));
}

/**
 * 缩略图内联为 data URL。
 * 真实 Tauri 用 asset 协议加载，浏览器 mock 里 file:// 被拦截，
 * 内联后即可验证缩略图渲染是否正常（而不是只看占位符）。
 * 注意：必须在 Node 侧完成（浏览器里没有文件系统）。
 *
 * thumb_path 约定为相对仓库根目录的路径（夹具随仓库走，不依赖临时目录）。
 */
function inlineThumbs(result) {
  let inlined = 0;
  // 大 fixture 会重复引用同一批缩略图（1000 组复制放大）——
  // 按 thumb_path 缓存 base64，避免重复读盘 + 巨量字符串注入
  const cache = new Map();
  for (const g of result.groups ?? []) {
    for (const f of g.files ?? []) {
      if (!f.thumb_path) continue;
      const original = f.thumb_path;
      if (cache.has(original)) {
        f.thumb_path = cache.get(original);
        continue;
      }
      try {
        const abs = resolve(ROOT, original);
        const buf = readFileSync(abs);
        f.thumb_path = `data:image/jpeg;base64,${buf.toString("base64")}`;
        inlined++;
      } catch {
        f.thumb_path = "";
      }
      cache.set(original, f.thumb_path);
    }
  }
  console.log(`[fixture] 缩略图内联 ${inlined} 张（去重后）`);
  return result;
}

/** 构造 mock 的 Tauri IPC（包成 IIFE，便于作为表达式注入页面） */
function mockScript(scenarioName) {
  const result = inlineThumbs(loadScanResult());
  return `(() => {
    window.__mockTrashed = [];
    window.__mockRestored = [];
    window.__mockCancelled = false;
    window.__TAURI_INTERNALS__ = {
      invoke: async (cmd, args) => {
        throw new Error("unexpected command: " + cmd);
      },
      transformCallback: (cb) => {
        const id = Math.floor(Math.random() * 1e9);
        window["_cb_" + id] = cb;
        return id;
      },
      convertFileSrc: (p) =>
        p.startsWith("data:") ? p : "asset://localhost/" + p.replace(/^\\//, ""),
    };
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
      unregisterListener: () => {},
    };
    // plugin-dialog 的 open() 也走 invoke("plugin:dialog|open")
    // 注意：不能引用 origInvoke —— core.js 的 invoke 内部会再取 window.__TAURI_INTERNALS__.invoke，
    // 形成自引用递归。这里直接在一个 invoke 里分支处理。
    window.__TAURI_INTERNALS__.invoke = async (cmd, args) => {
      if (cmd.startsWith("plugin:dialog|open")) return ${JSON.stringify("/tmp/photo_test2")};
      if (cmd.startsWith("plugin:event|listen")) return 1; // 监听 id
      if (cmd.startsWith("plugin:event|unlisten")) return null;
      switch (cmd) {
        case "start_scan":
          window.__mockScanArgs = args;
          if (${JSON.stringify(scenarioName)} === "cancel") {
            await new Promise((resolve) => setTimeout(resolve, 350));
            if (window.__mockCancelled) throw new Error("扫描已取消");
          }
          return ${JSON.stringify(result)};
        case "cancel_scan":
          window.__mockCancelled = true;
          return true;
        case "regroup":
          return ${JSON.stringify(result)};
        case "trash_files": {
          // 模拟后端行为：把勾选的文件从分组里剔除，重新算保留项和统计。
          // 只留一个"删除后"的分组快照，验证前端是否用返回值替换了整份 result。
          const paths = new Set(args?.paths ?? []);
          window.__mockTrashed.push(...paths);
          const keptGroups = ${JSON.stringify(result)}.groups
            .map((g) => ({
              kind: g.kind,
              files: g.files.filter((f) => !paths.has(f.path)),
            }))
            .filter((g) => g.files.length >= 2);
          // 重新算 is_best：剩下的第一张顶上（真实后端会按分辨率/大小重排，
          // mock 里简化为"剩下的第一张"，够用来验证 UI 更新）
          for (const g of keptGroups) {
            g.files.forEach((f, i) => { f.is_best = i === 0; });
          }
          const keptFiles = keptGroups.flatMap((g) => g.files);
          const dupes = keptFiles.filter((f) => !f.is_best);
          const items = [...paths].map((p) => ({
            original: p,
            trash_path: "/Users/example/.Trash/" + p.split("/").pop(),
            error: null,
          }));
          window.__mockTrashItems = items;
          return {
            items,
            batch_id: items.length > 0 ? 12345 : null,
            groups: keptGroups,
            stats: {
              ...${JSON.stringify(result)}.stats,
              exact_groups: keptGroups.filter((g) => g.kind === "exact").length,
              similar_groups: keptGroups.filter((g) => g.kind === "similar").length,
              duplicate_files: dupes.length,
              reclaimable_bytes: dupes.reduce((s, f) => s + f.size, 0),
            },
          };
        }
        case "restore_files": {
          // 撤销现在只传 batchId：恢复路径由后端批次注册表持有。
          // mock 记录批次 ID，并把上一次 trash 的条目全部恢复。
          window.__mockRestoredBatchId = args?.batchId ?? null;
          window.__mockRestored.push(
            ...(window.__mockTrashItems ?? []).map((i) => i.original),
          );
          return {
            items: (window.__mockTrashItems ?? []).map((i) => ({ ...i, error: null })),
            groups: ${JSON.stringify(result)}.groups,
            stats: ${JSON.stringify(result)}.stats,
          };
        }
        default:
          throw new Error("unexpected command: " + cmd);
      }
    };
    window.__SCENARIO__ = ${JSON.stringify(scenarioName)};
    window.__MOCK_LOADED__ = true;
    return true;
  })()`;
}

mkdirSync(OUT, { recursive: true });

/** MARKETING_DIR 设定时输出固定文件名（供 README 素材），否则写入 .shots/ 带时间戳 */
const MARKETING_DIR = process.env.MARKETING_DIR
  ? resolve(process.env.MARKETING_DIR)
  : null;

function shotPath(name) {
  if (MARKETING_DIR) {
    mkdirSync(MARKETING_DIR, { recursive: true });
    return resolve(MARKETING_DIR, name);
  }
  const base = name.replace(/\.png$/i, "");
  return `${OUT}/ui-${base}-${Date.now()}.png`;
}

// profile 放到工作区内：系统临时目录受沙箱限制，chromium 写不进去会被杀
const profileDir = resolve(ROOT, ".shots/.chromium-profile");
mkdirSync(profileDir, { recursive: true });

const context = await chromium.launchPersistentContext(profileDir, {
  executablePath: findChromium(),
  args: ["--no-sandbox", "--disable-dev-shm-usage", "--disable-gpu", "--allow-file-access-from-files"],
  viewport: { width: 1160, height: 780 },
  timeout: 30000,
});
const page = context.pages()[0] ?? (await context.newPage());

const errors = [];
page.on("console", (m) => {
  if (m.type() === "error") errors.push(m.text());
});
page.on("pageerror", (e) => errors.push(String(e)));

// 注入 mock：必须在页面脚本执行前挂上 window.__TAURI_INTERNALS__，
// 否则 @tauri-apps/api 在模块初始化时拿到 undefined（listen 报 transformCallback 错）
await page.addInitScript(mockScript(scenario));

await page.goto(BASE, { waitUntil: "networkidle" });

// 兜底：若 init script 未生效（个别 persistentContext 版本），再补一次
if (!(await page.evaluate(() => window.__MOCK_LOADED__ === true))) {
  console.log("[warn] addInitScript 未生效，回退到 evaluate 注入");
  await page.evaluate(mockScript(scenario));
}

if (scenario === "empty") {
  await page.waitForSelector(".empty-hero h1", { timeout: 10000 });
  await page.screenshot({ path: shotPath("empty.png") });
  console.log("[empty] 标题:", await page.textContent(".empty-hero h1"));
  console.log("[empty] 按钮:", await page.textContent(".empty-panel button.primary"));
} else if (scenario === "cancel") {
  await page.waitForSelector(".empty-panel button.primary", { timeout: 10000 });
  await page.click(".empty-panel button.primary");
  await page.waitForSelector(".cancel-btn", { timeout: 5000 });
  await page.screenshot({ path: shotPath("scanning.png") });
  const stageLabel = (await page.textContent(".scan-stage"))?.trim();
  if (!stageLabel || stageLabel === "正在处理") {
    throw new Error(`阶段文案未正常渲染: ${stageLabel}`);
  }
  console.log("[cancel] 阶段文案:", stageLabel);
  await page.click(".cancel-btn");
  await page.waitForSelector(".empty-panel .error", { timeout: 5000 });
  const cancelError = await page.textContent(".empty-panel .error");
  if (!cancelError.includes("扫描已取消")) {
    throw new Error(`取消后错误提示不符合预期: ${cancelError}`);
  }
  await page.screenshot({ path: shotPath("cancelled.png") });
  console.log("[cancel] 已返回空状态，提示:", cancelError.trim());
} else {
  // 结果页：直接触发扫描（绕过原生目录对话框）
  await page.waitForSelector(".empty-panel button.primary", { timeout: 10000 });
  page.on("console", (m) => console.log("  [browser]", m.text()));
  // 探针：直接调 mock invoke，确认 mock 生效
  const probe = await page.evaluate(async () => {
    try {
      if (!window.__MOCK_LOADED__) return { mockLoaded: false, err: "init script 未执行" };
      const r = await window.__TAURI_INTERNALS__.invoke("plugin:dialog|open", { options: {} });
      const s = await window.__TAURI_INTERNALS__.invoke("start_scan", {
        dir: "/x",
        includeSubdirs: true,
        level: "balanced",
      });
      return { dialog: r, scanGroups: s?.groups?.length, err: null };
    } catch (e) { return { err: String(e) }; }
  });
  console.log("[probe]", JSON.stringify(probe));
  await page.click(".empty-panel button.primary");
  await page.waitForTimeout(800);
  const stillEmpty = await page.$(".empty-panel button.primary");
  if (stillEmpty) {
    console.log("[warn] 仍停留在空状态，检查 mock");
  }

  // 前端必须把档位字符串传给后端（阈值数值已收敛到后端枚举）
  const scanArgs = await page.evaluate(() => window.__mockScanArgs ?? null);
  console.log("[results] start_scan 参数:", JSON.stringify(scanArgs));
  if (!scanArgs || scanArgs.level !== "balanced") {
    throw new Error("start_scan 未携带合法档位参数: " + JSON.stringify(scanArgs));
  }

  // mock 的 pickFolder 返回路径
  await page.waitForSelector(".results", { timeout: 15000 });
  await page.waitForTimeout(600);
  await page.screenshot({ path: shotPath("results.png") });

  const title = await page.textContent(".results-head h2");
  const summary = await page.textContent(".results-head .muted");
  const badges = await page.$$eval(".badge", (els) => els.map((e) => e.textContent.trim()));
  const groupCount = (await page.$$(".group")).length;
  const thumbCount = (await page.$$(".thumb")).length;
  const keepCount = (await page.$$(".thumb .keep-dot")).length;
  const sameTags = await page.$$eval(".content-tag.same", (els) => els.length);
  const diffTags = await page.$$eval(".content-tag.diff", (els) => els.length);
  // 每个 similar 组的非保留项都应带内容标记；exact 组不应带（组徽章已说明）
  // 「保留项」在前端用 .keep-dot 标记（绿点），不再是 .keep class
  const perGroupTags = await page.$$eval(".group", (groups) =>
    groups.map((g) => ({
      kind: g.querySelector(".badge")?.textContent.trim(),
      tags: g.querySelectorAll(".content-tag").length,
      nonBest: [...g.querySelectorAll(".thumb")].filter(
        (t) => !t.querySelector(".keep-dot")
      ).length,
    }))
  );
  const actionBar = await page.textContent(".action-info");
  const buttonState = await page.$eval(".action-bar button.danger", (e) => ({
    text: e.textContent.trim(),
    disabled: e.disabled,
  }));

  console.log("[results] 标题:", title.replace(/\s+/g, " ").trim());
  console.log("[results] 摘要:", summary.replace(/\s+/g, " ").trim());
  console.log("[results] 分组数:", groupCount, "缩略图数:", thumbCount);
  console.log("[results] 保留标记数:", keepCount, "内容一致:", sameTags, "仅相似:", diffTags);
  console.log("[results] 各组角标:", JSON.stringify(perGroupTags.slice(0, 3)), "…");
  console.log("[results] 徽章:", JSON.stringify(badges.slice(0, 6)), "…");
  console.log("[results] 操作条:", actionBar.replace(/\s+/g, " ").trim());
  console.log("[results] 删除按钮:", JSON.stringify(buttonState));

  // ---- 选择能力断言 ----
  // 每一张照片都必须有勾选框 —— 包括系统建议保留的那张。
  // 旧版只给「非保留项」渲染复选框，用户无法决定删哪张。
  const pickState = await page.$$eval(".thumb", (thumbs) =>
    thumbs.map((t) => ({
      hasPick: !!t.querySelector(".pick input"),
      checked: !!t.querySelector(".pick input")?.checked,
      isSuggestedKeep: !!t.querySelector(".keep-dot"),
    }))
  );
  const missingPick = pickState.filter((p) => !p.hasPick);
  if (missingPick.length) {
    throw new Error(`${missingPick.length} 张照片没有勾选框，用户无法选择`);
  }
  console.log(
    "[select] 勾选框覆盖:",
    `${pickState.length - missingPick.length}/${pickState.length}`,
    "默认勾选",
    pickState.filter((p) => p.checked).length,
    "张"
  );

  // 默认态：系统建议保留的那张不应被勾上
  for (const p of pickState) {
    if (p.isSuggestedKeep && p.checked) {
      throw new Error("系统建议保留的那张默认被勾选，应为未勾选");
    }
  }

  // 决策 #2（2026-10-28）：默认勾选只含「内容一致」的项。
  // 「仅相似」（.content-tag.diff）必须默认不勾 —— 误聚时默认删除风险过高。
  const defaultChecked = pickState.filter((p) => p.checked).length;
  const nonBestCount = pickState.filter((p) => !p.isSuggestedKeep).length;
  if (defaultChecked !== nonBestCount - diffTags) {
    throw new Error(
      `默认勾选应只含内容一致项：期望 ${nonBestCount - diffTags}，实际 ${defaultChecked}`
    );
  }
  console.log(
    "[select] 默认勾选（决策 #2）:",
    defaultChecked, "内容一致 ·", diffTags, "仅相似（须手动勾）"
  );

  // 勾选框与「删除」复选框（旧版 .pick 标签）不能重叠
  const overlap = await page.evaluate(() => {
    for (const tag of document.querySelectorAll(".content-tag")) {
      const pick = tag.closest(".thumb")?.querySelector(".pick");
      if (!pick) continue;
      const a = tag.getBoundingClientRect();
      const b = pick.getBoundingClientRect();
      if (a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom) {
        return true;
      }
    }
    return false;
  });
  if (overlap) {
    throw new Error("内容标记与勾选框重叠");
  }

  // 交互：点击「系统建议保留」那张的勾选框 → 选中数 +1（用户可以改主意删它）
  // 交互断言只对小 fixture 跑（大 fixture 上 3500 张逐项点击既慢又不增加覆盖）：
  // 渲染规模、默认勾选、lightbox、帧率由两组 fixture 共同覆盖。
  const heavyInteractions = groupCount <= 100;

  const before = await page.$eval(".action-info .num", (e) => Number(e.textContent));
  const keepIdx = pickState.findIndex((p) => p.isSuggestedKeep);
  if (keepIdx >= 0 && heavyInteractions) {
    await page.$$eval(
      ".thumb",
      (thumbs, i) => {
        thumbs[i].querySelector(".pick input").click();
      },
      keepIdx
    );
    await page.waitForTimeout(150);
    const after = await page.$eval(".action-info .num", (e) => Number(e.textContent));
    if (after !== before + 1) {
      throw new Error(`勾选「建议保留」项后选中数应 +1，实际 ${before} → ${after}`);
    }
    console.log("[select] 勾选建议保留项:", before, "→", after);
    // 再点一次取消
    await page.$$eval(
      ".thumb",
      (thumbs, i) => {
        thumbs[i].querySelector(".pick input").click();
      },
      keepIdx
    );
    await page.waitForTimeout(150);
  }

  // 交互：组级「全不选」→ 所有组清空；「全选」→ 所有组全选（小 fixture）
  const groupBtns = await page.$$(".group-actions button.link");
  if (!heavyInteractions) {
    console.log("[select] 大 fixture：跳过逐组点击交互（由小 fixture 覆盖）");
  }
  if (groupBtns.length < 2) {
    throw new Error("缺少组级「全选 / 全不选」按钮");
  }
  if (heavyInteractions) {
    const total0 = await page.$eval(".action-info .num", (e) => Number(e.textContent));
    // 每组的按钮顺序是 [全选, 全不选]，逐组点「全不选」
    for (let i = 1; i < groupBtns.length; i += 2) {
      await groupBtns[i].click();
    }
    await page.waitForTimeout(200);
    const afterNone = await page.$eval(".action-info .num", (e) => Number(e.textContent));
    if (afterNone !== 0) {
      throw new Error(`全部「全不选」后选中数应为 0，实际 ${afterNone}`);
    }
    // 再逐组点「全选」
    for (let i = 0; i < groupBtns.length; i += 2) {
      await groupBtns[i].click();
    }
    await page.waitForTimeout(200);
    const afterAll = await page.$eval(".action-info .num", (e) => Number(e.textContent));
    if (afterAll !== thumbCount) {
      throw new Error(`全部「全选」后选中数应为 ${thumbCount}，实际 ${afterAll}`);
    }
    console.log("[select] 组级全不选→全选:", total0, "→", afterNone, "→", afterAll);
  }
  // 复原默认态：与决策 #2 一致 —— 勾上「非保留且非仅相似」，取消其余
  if (heavyInteractions) {
    await page.$$eval(".thumb", (thumbs) => {
      for (const t of thumbs) {
        const input = t.querySelector(".pick input");
        const keepDot = t.querySelector(".keep-dot");
        const isDiff = !!t.querySelector(".content-tag.diff");
        const want = !keepDot && !isDiff;
        if (input && input.checked !== want) input.click();
      }
    });
    await page.waitForTimeout(250);
    const restored = await page.$eval(".action-info .num", (e) => Number(e.textContent));
    if (restored !== defaultChecked) {
      throw new Error(`复原默认态后选中数应为 ${defaultChecked}，实际 ${restored}`);
    }
  }

  // 断言：内容标记只出现在「几乎一样」的组，且覆盖该组全部非保留项
  for (const g of perGroupTags) {
    const isSimilar = g.kind === "几乎一样";
    if (isSimilar && g.tags !== g.nonBest) {
      throw new Error(
        `「几乎一样」组应有 ${g.nonBest} 个内容标记，实际 ${g.tags}`
      );
    }
    if (!isSimilar && g.tags !== 0) {
      throw new Error(`「${g.kind}」组不应显示逐张内容标记，实际 ${g.tags}`);
    }
  }
  if (sameTags < 1 || diffTags < 1) {
    throw new Error(
      `应同时存在「内容一致」与「仅相似」两种标记，实际 ${sameTags} / ${diffTags}`
    );
  }

  // ---- lightbox 大图对比（阶段 3）：点击缩略图放大 + Esc 关闭 ----
  await page.click(".group:first-child .thumb-img img");
  await page.waitForSelector(".lightbox-mask", { timeout: 5000 });
  const lightboxInfo = (await page.textContent(".lightbox-meta"))?.replace(/\s+/g, " ").trim();
  const lightboxPath = await page.textContent(".lightbox-path");
  if (!lightboxInfo || !lightboxPath) {
    throw new Error("lightbox 元信息或完整路径缺失");
  }
  await page.screenshot({ path: shotPath("lightbox.png") });
  await page.keyboard.press("Escape");
  await page.waitForSelector(".lightbox-mask", { state: "detached", timeout: 5000 });
  console.log("[lightbox] 大图对比:", lightboxInfo, "| 路径:", lightboxInfo && lightboxPath);

  // ---- 帧率采样（阶段 3：大结果集 DOM 实测，仅大 fixture 有意义）----
  const sampleFps = async () =>
    page.evaluate(async () => {
      let frames = 0;
      const t0 = performance.now();
      await new Promise((done) => {
        const tick = () => {
          frames++;
          if (performance.now() - t0 < 1000) requestAnimationFrame(tick);
          else done();
        };
        requestAnimationFrame(tick);
      });
      return frames;
    });
  const fpsIdle = await sampleFps();
  await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
  await page.waitForTimeout(400);
  const fpsScrolled = await sampleFps();
  console.log(`[perf] ${groupCount} 组 · 帧率: 静止 ~${fpsIdle} fps · 滚动到底 ~${fpsScrolled} fps`);
  if (groupCount >= 500) {
    // 1000 组实测门槛（docs/04 阶段 3 验收）：content-visibility 必须扛得住
    if (fpsIdle < 20 || fpsScrolled < 20) {
      throw new Error(`大结果集帧率不达标（静止 ${fpsIdle} / 滚动 ${fpsScrolled} < 20fps），需要真虚拟列表`);
    }
  }

  if (scenario === "trash-confirm") {
    await page.click(".action-bar button.danger");
    await page.waitForSelector(".modal", { timeout: 5000 });
    await page.screenshot({ path: shotPath("trash-confirm.png") });
    const modal = await page.textContent(".modal");
    console.log("[confirm] 弹窗:", modal.replace(/\s+/g, " ").trim());
  }

  if (scenario === "trash-undo") {
    // 端到端验证删除后 UI 状态自洽 + 撤销链路。
    const initialGroups = await page.$$eval(".group", (els) => els.length);
    const initialThumbs = await page.$$eval(".thumb", (els) => els.length);
    const initialSelected = await page.$eval(".action-info .num", (e) => Number(e.textContent));
    console.log("[undo] 初始:", { initialGroups, initialThumbs, initialSelected });

    // 触发删除（默认勾选 = 所有非保留项）
    await page.click(".action-bar button.danger");
    await page.waitForSelector(".modal", { timeout: 5000 });
    await page.click(".modal button.danger");

    // 删除后：撤销条出现，分组/缩略图数量应减少
    await page.waitForSelector(".undo-bar", { timeout: 5000 });
    await page.screenshot({ path: shotPath("after-trash.png") });
    const afterTrashGroups = await page.$$eval(".group", (els) => els.length);
    const afterTrashThumbs = await page.$$eval(".thumb", (els) => els.length);
    const undoText = (await page.textContent(".undo-bar .undo-text")).replace(/\s+/g, " ").trim();
    console.log("[undo] 删除后:", { afterTrashGroups, afterTrashThumbs, undoText });

    // 关键断言 1：被删的照片必须从列表消失
    if (afterTrashThumbs >= initialThumbs) {
      throw new Error(
        `删除后缩略图数应减少（${initialThumbs} → ${afterTrashThumbs}），实际没变或增加`
      );
    }
    // 关键断言 2：全部清完时应显示「all-clean」空态，而不是继续挂着空的操作条
    if (afterTrashGroups === 0) {
      const allClean = await page.$(".all-clean");
      if (!allClean) {
        throw new Error("分组归零后应显示 .all-clean 空态");
      }
      // 撤销条必须仍然可见（浮在空态之上）
      // 注意：position:fixed 元素的 offsetParent 恒为 null，不能用它判可见性
      const undoVisible = await page.$eval(".undo-bar", (e) => {
        const r = e.getBoundingClientRect();
        return r.width > 0 && r.height > 0;
      });
      if (!undoVisible) {
        throw new Error("分组归零后撤销条不应被空态挤掉");
      }
      console.log("[undo] 空态: .all-clean 已渲染，撤销条仍浮于其上");
    } else {
      // 还有剩余分组：操作条应保留，且勾选集合应被清空
      const afterTrashSelected = await page.$eval(".action-info .num", (e) => Number(e.textContent));
      if (afterTrashSelected !== 0) {
        throw new Error(`删除后选中数应为 0，实际 ${afterTrashSelected}`);
      }
    }
    // 关键断言 3：撤销条必须显示倒计时数字
    const countdown = await page.textContent(".undo-bar .undo-countdown");
    if (!/\d+s/.test(countdown?.trim() ?? "")) {
      throw new Error(`撤销条缺少倒计时，实际文案: ${countdown}`);
    }
    console.log("[undo] 倒计时初始:", countdown.trim());

    // 等 1.2s 验证倒计时真的在跑（不是静态文本）
    await page.waitForTimeout(1200);
    const countdownLater = await page.textContent(".undo-bar .undo-countdown");
    if (countdownLater?.trim() === countdown?.trim()) {
      throw new Error(`倒计时未在走动: ${countdown.trim()} → ${countdownLater?.trim()}`);
    }
    console.log("[undo] 倒计时走动:", countdown.trim(), "→", countdownLater?.trim());

    // 点击撤销
    await page.click(".undo-bar .undo-btn");
    // 撤销后：撤销条消失，分组恢复到初始状态
    await page.waitForSelector(".undo-bar", { state: "detached", timeout: 5000 });
    await page.waitForTimeout(300);
    await page.screenshot({ path: shotPath("after-undo.png") });
    const afterUndoGroups = await page.$$eval(".group", (els) => els.length);
    const afterUndoThumbs = await page.$$eval(".thumb", (els) => els.length);
    console.log("[undo] 撤销后:", { afterUndoGroups, afterUndoThumbs });

    // 关键断言 4：撤销后分组和缩略图数量必须回到初始值
    if (afterUndoGroups !== initialGroups) {
      throw new Error(
        `撤销后分组数应恢复到 ${initialGroups}，实际 ${afterUndoGroups}`
      );
    }
    if (afterUndoThumbs !== initialThumbs) {
      throw new Error(
        `撤销后缩略图数应恢复到 ${initialThumbs}，实际 ${afterUndoThumbs}`
      );
    }
    // 关键断言 5：mock 必须收到带批次 ID 的 restore_files 调用，且与 trash 返回的批次一致
    const restored = await page.evaluate(() => window.__mockRestored ?? []);
    const trashedCount = await page.evaluate(() => (window.__mockTrashed ?? []).length);
    if (restored.length !== trashedCount) {
      throw new Error(
        `撤销恢复的文件数 (${restored.length}) 应等于删除的文件数 (${trashedCount})`
      );
    }
    const restoredBatchId = await page.evaluate(() => window.__mockRestoredBatchId ?? null);
    if (restoredBatchId !== 12345) {
      throw new Error(`restore_files 应携带 trash_files 返回的批次 ID，实际 ${restoredBatchId}`);
    }
    console.log("[undo] 撤销链路:", { trashedCount, restoredCount: restored.length, restoredBatchId });
  }
}

if (errors.length) {
  console.log("[console errors]", JSON.stringify(errors.slice(0, 5), null, 2));
} else {
  console.log("[console errors] 无");
}

await context.close();
