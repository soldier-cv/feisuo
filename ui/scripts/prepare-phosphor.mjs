/**
 * 生成精简版 Phosphor 图标字体资源到 public/phosphor/
 *
 * 背景: 直接 `import "@phosphor-icons/web/regular"` 会让打包器把
 * ttf / woff / svg 三种字体格式全部塞进产物, 光 SVG 字体就有 ~8MB。
 * 局域网传输工具要的是小体积安装包, 因此这里只保留 woff2。
 *
 * 另外, 此前图标 CSS 是从 CDN 引入的, 一旦断网整个界面图标会全部变成方块。
 */
import { readFileSync, writeFileSync, mkdirSync, copyFileSync, existsSync, rmSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const uiDir = resolve(__dirname, "..");
const srcRoot = join(uiDir, "node_modules", "@phosphor-icons", "web", "src");
const outDir = join(uiDir, "public", "phosphor");

/** 每套字体的实际文件名可能带后缀 (Phosphor / Phosphor-Fill / Phosphor-Bold) */
const WEIGHTS = [
  { key: "regular", font: "Phosphor" },
  { key: "fill", font: "Phosphor-Fill" },
  { key: "bold", font: "Phosphor-Bold" },
];

if (existsSync(outDir)) rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

for (const { key, font } of WEIGHTS) {
  const srcCss = join(srcRoot, key, "style.css");
  if (!existsSync(srcCss)) {
    console.error(`[phosphor] 缺少 ${srcCss}, 请先执行 pnpm install`);
    process.exit(1);
  }

  const raw = readFileSync(srcCss, "utf8");

  // 只保留 woff2 引用, 去掉 ttf / woff / svg 三份冗余字体
  const slim = raw.replace(
    new RegExp(
      `src:\\s*url\\("\\./${font}\\.woff2"\\) format\\("woff2"\\),\\s*` +
        `url\\("\\./${font}\\.woff"\\) format\\("woff"\\),\\s*` +
        `url\\("\\./${font}\\.ttf"\\) format\\("truetype"\\),\\s*` +
        `url\\("\\./${font}\\.svg#${font}"\\) format\\("svg"\\);`
    ),
    `src: url("./${font}.woff2") format("woff2");`
  );

  if (slim === raw || !slim.includes('format("woff2")')) {
    console.error(`[phosphor] ${key}/style.css 的 @font-face 结构与预期不符, 已中止`);
    process.exit(1);
  }
  if (slim.includes(".svg#")) {
    console.error(`[phosphor] ${key}: SVG 字体引用未被清除, 已中止`);
    process.exit(1);
  }

  writeFileSync(join(outDir, `${key}.css`), slim, "utf8");
  copyFileSync(join(srcRoot, key, `${font}.woff2`), join(outDir, `${font}.woff2`));

  const kb = (slim.length / 1024).toFixed(0);
  console.log(`[phosphor] ${key}: css=${kb}KB woff2=已复制`);
}

console.log("[phosphor] 完成 -> public/phosphor/");
