// Enhancements only: the complete reference and table of contents work without JavaScript.
"use strict";

document.querySelectorAll("[data-js]").forEach((element) => { element.hidden = false; });

const filter = document.getElementById("key-filter");
const rows = [...document.querySelectorAll("[data-config-key]")];
const status = document.getElementById("filter-status");
filter.disabled = false;
filter.addEventListener("input", () => {
  const query = filter.value.trim().toLowerCase();
  let shown = 0;
  rows.forEach((row) => {
    row.hidden = query !== "" && !row.dataset.configKey.includes(query);
    if (!row.hidden) shown += 1;
  });
  status.textContent = query ? `${shown} of ${rows.length} keys shown.` : `${rows.length} keys shown.`;
});
status.textContent = `${rows.length} keys shown.`;

document.querySelectorAll(".code-copy").forEach((button) => {
  button.addEventListener("click", async () => {
    const code = button.parentElement.querySelector("code");
    try {
      await navigator.clipboard.writeText(code.textContent);
      button.textContent = "copied";
    } catch {
      const range = document.createRange();
      range.selectNodeContents(code);
      const selection = getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      button.textContent = "select";
    }
    setTimeout(() => { button.textContent = "copy"; }, 1400);
  });
});
