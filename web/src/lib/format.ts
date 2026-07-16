export function humanSize(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let f = n / 1024;
  let i = 0;
  while (f >= 1024 && i < units.length - 1) {
    f /= 1024;
    i += 1;
  }
  return `${f.toFixed(1)} ${units[i]}`;
}

export function shortHash(id: string): string {
  // "blake3:9f2a…" -> "9f2a…c71"
  const hex = id.includes(":") ? id.split(":")[1] : id;
  return hex.length > 12 ? `${hex.slice(0, 6)}…${hex.slice(-4)}` : hex;
}
