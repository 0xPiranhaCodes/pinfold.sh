// Enhancements only: the page is complete without this file.
"use strict";
document.documentElement.classList.add("js");

const $ = (id) => document.getElementById(id);
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
const still = matchMedia("(prefers-reduced-motion: reduce)").matches;

// The title row's clock, in the app's format: "Wed 04 Nov  14:22".
function tick() {
  const d = new Date();
  const part = (o) => d.toLocaleString("en-GB", o);
  $("clock").textContent = `${part({ weekday: "short" })} ${part({ day: "2-digit" })} ${part({ month: "short" })}  ${part({ hour: "2-digit", minute: "2-digit" })}`;
}
tick();
setInterval(tick, 10000);

// Hero: type the commands of the real session, print each output at once.
const term = $("term");
if (!still) {
  const done = term.innerHTML;
  const lines = done.split("\n");
  term.innerHTML = "";
  let shown = "";
  const step = (i) => {
    if (i >= lines.length) return void (term.innerHTML = done);
    const line = lines[i];
    const command = line.match(/^(<span class="p">\$<\/span> )(.+)$/);
    if (!command || line.includes("cursor")) {
      shown += `${line}\n`;
      term.innerHTML = shown;
      return void setTimeout(step, 60, i + 1);
    }
    let n = 0;
    const type = () => {
      term.innerHTML = `${shown}${command[1]}${command[2].slice(0, n)}<span class="cursor"> </span>`;
      if (n++ < command[2].length) return void setTimeout(type, 35 + Math.random() * 45);
      shown += `${line}\n`;
      setTimeout(step, 400, i + 1);
    };
    type();
  };
  setTimeout(step, 600, 0);
}

// The site shell. Output is either verbatim from the binary or from this page's own text.
const usage = $("out").textContent.split("\n").find((l) => l.startsWith("usage: pinfold"));
const shell = {
  help: () => "help  pinfold --help  states  keys  mem  install  ls  cd <section>  clear",
  "pinfold --help": () => usage,
  states: () => [...document.querySelectorAll(".states li")].map((li) => { const [glyph, word, what] = li.children; return `${glyph.textContent} ${word.textContent.padEnd(10)} ${what.textContent}`; }).join("\n"),
  keys: () => [...document.querySelectorAll("#workflow .keys dt")].map((dt) => `${dt.textContent.padEnd(14)}${dt.nextElementSibling.textContent}`).join("\n"),
  mem: () => [...document.querySelectorAll("#memory tbody tr")].slice(0, 7).map((tr) => { const c = tr.cells; return `${c[0].textContent.padEnd(26)}${c[1].textContent.padStart(6)} / ${c[2].textContent} MB`; }).join("\n") + "\nmacOS, M1 Pro, PR #169",
  install: () => "macOS beta: coming soon, on GitHub Releases\nfrom source: cargo build --release -p pinfold-cli",
  ls: () => [...document.querySelectorAll("main section")].map((s) => `${s.id}/`).join("  "),
};
shell["pinfold -h"] = shell["pinfold help"] = shell["pinfold --help"];

const out = $("out");
const input = $("cmd");
const history = [];
let back = 0;
$("shell-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const raw = input.value.trim().replace(/\s+/g, " ");
  input.value = "";
  if (!raw) return;
  history.push(raw);
  back = history.length;
  if (raw === "clear") return void (out.innerHTML = "");
  let reply;
  const cd = raw.match(/^cd (\w+)\/?$/);
  if (cd && $(cd[1])?.matches("main section")) {
    $(cd[1]).scrollIntoView({ behavior: still ? "auto" : "smooth" });
    reply = "";
  } else if (shell[raw]) {
    reply = esc(shell[raw]());
  } else {
    reply = `<span class="err">✕ error</span> ${esc(raw.split(" ")[0])}: not on this page. Try help`;
  }
  out.innerHTML += `<span class="p">$</span> ${esc(raw)}\n${reply}${reply ? "\n" : ""}`;
  out.scrollTop = out.scrollHeight;
});
input.addEventListener("keydown", (event) => {
  if (event.key === "ArrowUp" && back > 0) input.value = history[--back];
  else if (event.key === "ArrowDown") input.value = history[(back = Math.min(history.length, back + 1))] ?? "";
  else if (event.key === "Escape") input.blur();
  else return;
  event.preventDefault();
});

// Page keys: 1 to 7 pick a tab, "/" types in the shell. Nothing fires while typing.
const tabs = [...document.querySelectorAll(".tabs a")];
document.addEventListener("keydown", (event) => {
  if (event.ctrlKey || event.metaKey || event.altKey || event.target.closest("input, textarea")) return;
  const tab = tabs[Number(event.key) - 1];
  if (tab) {
    tab.click();
    tab.focus();
  } else if (event.key === "/") {
    event.preventDefault();
    input.focus();
  }
});

// The tab of the section in view is the active one.
const seen = new IntersectionObserver((entries) => {
  for (const entry of entries) {
    if (!entry.isIntersecting) continue;
    for (const tab of tabs) tab.setAttribute("aria-current", tab.hash === `#${entry.target.id}` ? "true" : "false");
  }
}, { rootMargin: "-40% 0px -55% 0px" });
document.querySelectorAll("main section").forEach((s) => seen.observe(s));
