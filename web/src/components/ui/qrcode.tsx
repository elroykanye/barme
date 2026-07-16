import { useMemo } from "react";
import { qrMatrix } from "@/lib/qr";

/** Renders text as a scannable QR code (inline SVG). Falls back silently. */
export function QRCode({
  value,
  size = 176,
  className,
}: {
  value: string;
  size?: number;
  className?: string;
}) {
  const matrix = useMemo(() => {
    try {
      return qrMatrix(value, "MEDIUM");
    } catch {
      return null;
    }
  }, [value]);

  if (!matrix) return null;

  const n = matrix.length;
  const quiet = 2;
  const dim = n + quiet * 2;

  let path = "";
  for (let y = 0; y < n; y++) {
    for (let x = 0; x < n; x++) {
      if (matrix[y][x]) path += `M${x + quiet},${y + quiet}h1v1h-1z`;
    }
  }

  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${dim} ${dim}`}
      className={className}
      shapeRendering="crispEdges"
    >
      <rect width={dim} height={dim} fill="#ffffff" />
      <path d={path} fill="#000000" />
    </svg>
  );
}
