#!/usr/bin/env node
/* The theme legs of the end-to-end run, driven over the DevTools
   protocol. The headless shell's CLI flags cannot emulate the OS
   colour scheme (neither --force-prefers-color-scheme nor
   --force-dark-mode moves matchMedia there), so these legs speak
   CDP directly — Emulation.setEmulatedMedia is the honest way to
   hand the page a dark or a light OS. No dependencies: Node's
   built-in fetch and WebSocket only.
 *
 * Usage: node scripts/cdp.mjs <chrome-bin> <base-url> <work-dir> <shots-dir>
 * Expects the e2e bundle to be published already (the shell script
 * runs these legs after its publish legs). Prints PASS/FAIL lines
 * and exits non-zero on any failure. */
import { spawn } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const [chromeBin, base, workDir, shotsDir] = process.argv.slice(2);
let failures = 0;
function check(name, cond, detail = "") {
  console.log(`${cond ? "PASS" : "FAIL"} ${name}${cond ? "" : " — " + detail}`);
  if (!cond) failures++;
}

async function launch(profile) {
  const proc = spawn(chromeBin, [
    "--no-sandbox", "--disable-gpu", "--hide-scrollbars",
    "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank",
  ], { stdio: "ignore" });
  const portFile = `${profile}/DevToolsActivePort`;
  let port = null;
  for (let i = 0; i < 100 && !port; i++) {
    if (existsSync(portFile)) port = readFileSync(portFile, "utf8").split("\n")[0].trim();
    else await sleep(100);
  }
  if (!port) throw new Error("chrome never wrote DevToolsActivePort");
  const version = await (await fetch(`http://127.0.0.1:${port}/json/version`)).json();
  const ws = new WebSocket(version.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let nextId = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  };
  const send = (method, params = {}, sessionId) => new Promise((res) => {
    const id = ++nextId;
    pending.set(id, res);
    ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
  });
  return { proc, send };
}

async function openPage(browser, { dark = false, width = 1600, height = 1000 } = {}) {
  const created = await browser.send("Target.createTarget", { url: "about:blank" });
  const attached = await browser.send("Target.attachToTarget", {
    targetId: created.result.targetId, flatten: true,
  });
  const sessionId = attached.result.sessionId;
  const send = (method, params = {}) => browser.send(method, params, sessionId);
  await send("Page.enable");
  await send("Emulation.setDeviceMetricsOverride", {
    width, height, deviceScaleFactor: 1, mobile: false,
  });
  if (dark) {
    await send("Emulation.setEmulatedMedia", {
      features: [{ name: "prefers-color-scheme", value: "dark" }],
    });
  }
  return { send };
}

async function goto(page, url) {
  await page.send("Page.navigate", { url });
  for (let i = 0; i < 80; i++) {
    if ((await evaluate(page, "document.readyState")) === "complete") break;
    await sleep(150);
  }
  await sleep(600); // let the faces render and the fonts settle
}

async function evaluate(page, expression) {
  const msg = await page.send("Runtime.evaluate", { expression, returnByValue: true });
  const result = msg.result && msg.result.result;
  if (result && result.exceptionDetails) {
    throw new Error("page evaluation failed: " + expression);
  }
  return result ? result.value : undefined;
}

async function screenshot(page, path) {
  const msg = await page.send("Page.captureScreenshot", { format: "png" });
  writeFileSync(path, Buffer.from(msg.result.data, "base64"));
}

const themeOf = (page) => evaluate(page, `document.documentElement.getAttribute("data-theme")`);
const bodyBg = (page) => evaluate(page, `getComputedStyle(document.body).backgroundColor`);

/* ——— a dark OS ——— */
{
  const browser = await launch(`${workDir}/profile-dark-os`);
  try {
    const editor = await openPage(browser, { dark: true, width: 1600, height: 1000 });
    await goto(editor, `${base}/?demo`);
    check("theme e2e: dark OS, no choice — the editor opens dark",
      (await themeOf(editor)) === "dark" && (await bodyBg(editor)) === "rgb(26, 25, 21)",
      `${await themeOf(editor)} / ${await bodyBg(editor)}`);
    await screenshot(editor, `${shotsDir}/06-editor-dark.png`);

    const reading = await openPage(browser, { dark: true, width: 1200, height: 1000 });
    await goto(reading, `${base}/b/e2e-bundle/essays/one`);
    check("theme e2e: dark OS, no choice — a published page opens dark",
      (await themeOf(reading)) === "dark" && (await bodyBg(reading)) === "rgb(26, 25, 21)",
      `${await themeOf(reading)} / ${await bodyBg(reading)}`);
    await screenshot(reading, `${shotsDir}/07-published-dark.png`);

    // The override: this browser chooses light, under a dark OS.
    await goto(editor, `${base}/`);
    await evaluate(editor, `localStorage.setItem("mdwiz.theme", "light")`);
    await goto(editor, `${base}/`);
    check("theme e2e: dark OS, light choice — the editor opens light",
      (await themeOf(editor)) === "light" && (await bodyBg(editor)) === "rgb(247, 246, 241)",
      `${await themeOf(editor)} / ${await bodyBg(editor)}`);
    await screenshot(editor, `${shotsDir}/08-editor-override.png`);
  } finally {
    browser.proc.kill();
  }
}

/* ——— a light OS ——— */
{
  const browser = await launch(`${workDir}/profile-light-os`);
  try {
    const editor = await openPage(browser, { width: 1600, height: 1000 });
    await goto(editor, `${base}/`);
    check("theme e2e: light OS, no choice — the editor opens light",
      (await themeOf(editor)) === "light" && (await bodyBg(editor)) === "rgb(247, 246, 241)",
      `${await themeOf(editor)} / ${await bodyBg(editor)}`);

    await evaluate(editor, `localStorage.setItem("mdwiz.theme", "dark")`);
    await goto(editor, `${base}/`);
    check("theme e2e: light OS, dark choice — the editor opens dark",
      (await themeOf(editor)) === "dark" && (await bodyBg(editor)) === "rgb(26, 25, 21)",
      `${await themeOf(editor)} / ${await bodyBg(editor)}`);

    const reading = await openPage(browser, { width: 1200, height: 1000 });
    await goto(reading, `${base}/b/e2e-bundle/essays/one`);
    check("theme e2e: a published page honours the same stored choice",
      (await themeOf(reading)) === "dark" && (await bodyBg(reading)) === "rgb(26, 25, 21)",
      `${await themeOf(reading)} / ${await bodyBg(reading)}`);
  } finally {
    browser.proc.kill();
  }
}

if (failures) {
  console.error(`theme e2e: ${failures} check(s) failed`);
  process.exit(1);
}
console.log("theme e2e: all checks passed");
