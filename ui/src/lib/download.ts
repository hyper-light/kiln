/** Files kiln hands you from what a page shows: a log as text, a table as CSV. */

/** Save `text` as a file named `name`, as the browser downloads one. */
export function saveFile(name: string, text: string, type = "text/plain;charset=utf-8") {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Give the browser time to start reading the object URL before releasing it.
  window.setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

/** When, for a file's name: 20261020-2153. */
export function fileStamp(d = new Date()): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}`;
}

/** Rows as CSV: the header, then a line per row, a field quoted where it holds a comma, a quote or a line break. */
export function toCsv(head: string[], rows: (string | number | null | undefined)[][]): string {
  const field = (v: string | number | null | undefined) => {
    const s = v == null ? "" : String(v);
    return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
  };
  return [head, ...rows].map((r) => r.map(field).join(",")).join("\r\n") + "\r\n";
}
