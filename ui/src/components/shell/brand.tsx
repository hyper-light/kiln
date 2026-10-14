// Path data from hyperlight-site lib/brand.ts — the optical contour mark.
export const brandContour =
  "M6.4 22.5C2.4 17.1 7.1 6.2 14 4.5C21.4 2.7 27.4 10.1 25.1 16.5C22.8 22.9 13.1 28.8 8.8 26.6";
export const brandFold =
  "M9.1 24.2C5.4 20.7 10.7 9.1 17.2 8.1C23.7 7.1 29.7 13.5 27 19.3C24.5 24.7 17.4 27.9 12.8 26.8";
export const brandRefraction = "M8.8 26.6C6.9 25.6 5.6 23.6 5.2 21.3";

export function BrandMark({ className = "brand-mark" }: { className?: string }) {
  return (
    <svg className={className} viewBox="0 0 32 32" fill="none" aria-hidden="true" focusable="false">
      <path d={brandContour} stroke="currentColor" strokeWidth="2" strokeLinecap="square" />
      <path d={brandFold} stroke="currentColor" strokeWidth="1.5" strokeLinecap="square" opacity=".56" />
      <path d={brandRefraction} stroke="#bcb1d8" strokeWidth="2" />
      <path d="M5.2 21.3C4.9 19.8 5.1 18.5 5.4 17.2" stroke="#aacbd0" strokeWidth="2" />
    </svg>
  );
}

/** Lowercase wordmark in the hyperlight voice: Geist 550, tight tracking. */
export function Brand({ wordmark = true }: { wordmark?: boolean }) {
  return (
    <span className="brand">
      <BrandMark />
      {wordmark && <span>kiln</span>}
    </span>
  );
}
