import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const iframe = document.getElementById("appFrame");
const boot = document.getElementById("boot");
const bootSpinner = document.getElementById("bootSpinner");
const bootStatus = document.getElementById("bootStatus");
const bootError = document.getElementById("bootError");
const fab = document.getElementById("fab");
const modal = document.getElementById("configModal");
const cfgDot = document.getElementById("cfgDot");
const cfgStatus = document.getElementById("cfgStatus");
const cfgAutostart = document.getElementById("cfgAutostart");

const HARNESS_URL = "http://127.0.0.1:3080";

let status = null;
// Only (re)load the iframe when the server transitions into running, so
// opening the config dialog or status polls never cause a page reload.
let iframeLoaded = false;

function setBoot(state, text, error) {
  bootSpinner.classList.toggle("hidden", state !== "starting");
  bootStatus.textContent = text;
  bootError.textContent = error ?? "";
  bootError.classList.toggle("visible", !!error);
}

function setCfg(state, text, error) {
  cfgDot.className = "dot " + state;
  cfgStatus.textContent = "";
  if (error) {
    const err = document.createElement("span");
    err.className = "err";
    err.textContent = error;
    cfgStatus.appendChild(err);
  } else {
    cfgStatus.textContent = text;
  }
}

function applyStatus(payload) {
  status = payload;
  if (payload.state === "running") {
    // Hide the boot page and point the iframe at the harness.
    boot.classList.add("hidden");
    if (!iframeLoaded) {
      iframe.src = HARNESS_URL + "?_t=" + Date.now();
      iframeLoaded = true;
    }
    setCfg("running", `运行中 · ${payload.url}`);
    setBoot("running", "服务已就绪", null);
  } else if (payload.state === "starting") {
    iframeLoaded = false;
    boot.classList.remove("hidden");
    setCfg("starting", `启动中（${payload.elapsedSecs ?? 0} 秒）…`);
    setBoot("starting", `正在启动本地服务（${payload.elapsedSecs ?? 0} 秒）…`, null);
  } else if (payload.state === "error") {
    iframeLoaded = false;
    boot.classList.remove("hidden");
    const msg = payload.error ?? "未知错误";
    setCfg("error", null, "启动失败：" + msg);
    setBoot("error", "启动失败", msg);
  } else {
    setCfg("idle", "空闲");
    setBoot("idle", "准备中…", null);
  }
}

function applyAutostart(enabled) {
  if (cfgAutostart.checked !== enabled) {
    cfgAutostart.checked = enabled;
  }
}

// events pushed from Rust
listen("server-status", (e) => applyStatus(e.payload)).catch(() => {});
listen("autostart-changed", (e) => applyAutostart(e.payload)).catch(() => {});
listen("upgrade-status", (e) => {
  const s = e.payload;
  if (s.state === "started") {
    setBoot("starting", "正在升级 dsh 并重启服务…", null);
  } else if (s.state === "done") {
    setBoot("starting", "升级完成，正在启动服务（首次可能需下载 1-2 分钟）…", null);
  } else if (s.state === "error") {
    setBoot("error", "升级失败", s.message ?? "未知错误");
  }
}).catch(() => {});

// initial state
invoke("get_status").then(applyStatus).catch(() => {});
invoke("get_autostart").then(applyAutostart).catch(() => {});

// ---- round icon: open the center config dialog (button hidden via CSS,
// kept so the dialog can be opened programmatically if needed) ----
fab.addEventListener("click", () => {
  modal.classList.add("open");
  invoke("get_status").then(applyStatus).catch(() => {});
});
function closeModal() {
  modal.classList.remove("open");
}
document.getElementById("cfgClose").addEventListener("click", closeModal);
modal.addEventListener("click", (e) => {
  if (e.target === modal) closeModal();
});

// ---- dialog actions ----
async function restart() {
  setCfg("starting", "正在重启服务…");
  try {
    await invoke("restart_server");
    const s = await invoke("get_status");
    applyStatus(s);
  } catch (e) {
    setCfg("error", null, "重启失败：" + String(e));
  }
}
document.getElementById("cfgRestart").addEventListener("click", restart);
document.getElementById("bootRetry").addEventListener("click", restart);

document.getElementById("cfgBrowser").addEventListener("click", async () => {
  try {
    await invoke("open_in_browser");
  } catch (e) {
    setCfg("error", null, "打开浏览器失败：" + String(e));
  }
});
document.getElementById("bootBrowser").addEventListener("click", async () => {
  try {
    await invoke("open_in_browser");
  } catch (e) {
    setBoot("error", "打开浏览器失败", String(e));
  }
});

cfgAutostart.addEventListener("change", async () => {
  try {
    const enabled = await invoke("set_autostart", { enabled: cfgAutostart.checked });
    applyAutostart(enabled);
  } catch (e) {
    setCfg("error", null, "设置自启动失败：" + String(e));
    applyAutostart(!cfgAutostart.checked);
  }
});

document.getElementById("cfgQuit").addEventListener("click", () => {
  invoke("quit_app").catch(() => {});
});

// fallback polling
setInterval(async () => {
  try {
    const s = await invoke("get_status");
    if (s.state !== status?.state) applyStatus(s);
  } catch {
    /* ignore */
  }
}, 3000);
