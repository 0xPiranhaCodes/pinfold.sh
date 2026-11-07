// Enhancements only: the page is complete without this file.
"use strict";
document.querySelectorAll("[data-js]").forEach((el) => { el.hidden = false; });

const $ = (id) => document.getElementById(id);
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);

// The site shell. `pinfold --help` is the binary's own text, already in the page.
const out = $("out");
const input = $("cmd");
const usage = out.textContent.slice(out.textContent.indexOf("PinFold: a memory-first")).trimEnd();
const html = new Set(["states", "sudo"]);
const cmds = {
  help: () => "commands: pinfold --help · keys · states · mem · install · about · clear",
  "pinfold --help": () => usage,
  keys: () => "⌘T new tab\n⌘D split right\n⇧⌘D split down\nCtrl+a, then c · | · -   tab · right · down",
  states: () => '<span class="st-needs">◆ needs you</span>  <span class="st-run">● running</span>  <span class="st-done">✓ done</span>  <span class="st-err">✕ error</span>  <span class="st-idle">○ idle</span>',
  mem: () => [...document.querySelectorAll("#memory tbody tr")].map((tr) => { const c = tr.cells; return `${c[0].textContent.padEnd(26)}${c[2].textContent.padStart(9)} / ${c[3].textContent.padEnd(9)} (${c[1].textContent})`; }).join("\n") + "\nreproduce: cargo xtask mem-gate",
  install: () => "macOS beta: PinFold.dmg from github.com/0xPiranhaCodes/pinfold.sh/releases/tag/v0.1.0-beta.1\nfrom source: cargo build --release -p pinfold-cli",
  about: () => "PinFold: a memory-first terminal for agentic coding, written in Rust.",
  ls: () => "why/  memory/  agents/  try/  install/",
  whoami: () => "you, the one the ◆ prompt is for",
  sudo: () => '<span class="st-err">✕ error</span>  agents ask first. so do you.',
};
cmds["pinfold -h"] = cmds["pinfold help"] = cmds["pinfold --help"];
const history = [];
let h = 0;
$("repl").addEventListener("submit", (e) => {
  e.preventDefault();
  const raw = input.value.trim().replace(/\s+/g, " ");
  input.value = "";
  if (!raw) return;
  history.push(raw);
  h = history.length;
  if (raw === "clear") return void (out.innerHTML = "");
  const f = cmds[raw];
  const res = f ? (html.has(raw) ? f() : esc(f())) : `<span class="st-err">✕</span> ${esc(raw)}: not found. try <span class="p">help</span>`;
  out.innerHTML += `<span class="p">~/pinfold.sh ❯</span> ${esc(raw)}\n${res}\n`;
  out.scrollTop = out.scrollHeight;
});
input.addEventListener("keydown", (e) => {
  if (e.key === "ArrowUp" && h > 0) { input.value = history[--h]; e.preventDefault(); }
  if (e.key === "ArrowDown") { h = Math.min(history.length, h + 1); input.value = history[h] || ""; e.preventDefault(); }
  if (e.key === "Tab" && input.value) {
    const m = Object.keys(cmds).find((k) => k.startsWith(input.value));
    if (m) { input.value = m; e.preventDefault(); }
  }
});
document.querySelectorAll(".hint .k").forEach((k) => {
  const pick = () => { input.value = k.textContent; input.focus(); };
  k.addEventListener("click", pick);
  k.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(); } });
});

// Copy the build command.
const copyBtn = $("copy-btn");
copyBtn.addEventListener("click", async () => {
  try { await navigator.clipboard.writeText($("src-cmd").textContent); copyBtn.textContent = "copied"; }
  catch { const r = document.createRange(); r.selectNodeContents($("src-cmd")); const s = getSelection(); s.removeAllRanges(); s.addRange(r); copyBtn.textContent = "select"; }
  setTimeout(() => { copyBtn.textContent = "copy"; }, 1400);
});

// Tab strip: scroll-spy, and ⌘1..6 or Ctrl+1..6.
const links = [...document.querySelectorAll("#tabs a")];
const targets = links.map((a) => document.querySelector(a.hash)).filter(Boolean);
const setCurrent = (id) => links.forEach((a) => (a.hash === `#${id}` ? a.setAttribute("aria-current", "page") : a.removeAttribute("aria-current")));
if ("IntersectionObserver" in window) {
  const io = new IntersectionObserver((es) => {
    const vis = es.filter((e) => e.isIntersecting).sort((a, b) => a.boundingClientRect.top - b.boundingClientRect.top)[0];
    if (vis) setCurrent(vis.target.id);
  }, { rootMargin: "-25% 0px -60% 0px" });
  targets.forEach((t) => io.observe(t));
}
addEventListener("keydown", (e) => {
  if (!(e.metaKey || e.ctrlKey) || e.target === input) return;
  const a = links.find((l) => l.dataset.key === e.key);
  if (a) { e.preventDefault(); a.click(); }
});

// Hero: replay the session; the static markup is the resting state.
const term = $("term");
if (!matchMedia("(prefers-reduced-motion: reduce)").matches) {
  const finalHTML = term.innerHTML;
  const p = '<span class="p">~/code/api ❯</span> ';
  const m = (s) => [`<span class="m">${s}</span>`, ""];
  const lines = [[p, "cat .pinfold/layout.toml"], m("[[tabs]]"), m("[[tabs.panes]]"), m('run = "cargo run"'), m("[[tabs.panes]]"), m('split = "right"'), m('run = "claude"'), [p, "pinfold up"]];
  let i = 0, j = 0, buf = "";
  const cursor = '<span class="cursor">&nbsp;</span>';
  term.innerHTML = cursor;
  const tick = () => {
    if (i >= lines.length) return void setTimeout(() => { term.innerHTML = `${finalHTML}\n${cursor}`; }, 300);
    const [pre, typed] = lines[i];
    if (j === 0) buf += pre;
    if (j < typed.length) { buf += esc(typed[j++]); term.innerHTML = buf + cursor; return void setTimeout(tick, 65); }
    buf += "\n"; i++; j = 0; term.innerHTML = buf + cursor; setTimeout(tick, typed ? 380 : 120);
  };
  setTimeout(tick, 450);
}
