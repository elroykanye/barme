/**
 * The barme pot mark: a pot (lid + body) in the brand accent with a small merkle
 * tree cut out of it — content-addressed storage as a picture. Theme-adaptive:
 * the pot takes the accent, the cutout takes the page background, both driven by
 * the CSS design tokens so it tracks light/dark. Matches the marketing site.
 */
export function Logo({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 32 32" className={className} role="img" aria-label="barme">
      <rect x="6.5" y="6.5" width="19" height="4" rx="2" fill="var(--color-accent)" />
      <path
        d="M8.5 12 L23.5 12 C25 12 25.4 13.5 25.2 15.2 C24.8 19 24.6 21.5 23 23.5 C21.8 25 20 25.5 16 25.5 C12 25.5 10.2 25 9 23.5 C7.4 21.5 7.2 19 6.8 15.2 C6.6 13.5 7 12 8.5 12 Z"
        fill="var(--color-accent)"
      />
      <g stroke="var(--color-bg)" strokeWidth="1.3" strokeLinecap="round" fill="none">
        <path d="M16 15 L12.5 20 M16 15 L19.5 20 M16 15 L16 20" />
      </g>
      <g fill="var(--color-bg)">
        <circle cx="16" cy="14.6" r="1.5" />
        <circle cx="12.5" cy="20.4" r="1.5" />
        <circle cx="16" cy="20.4" r="1.5" />
        <circle cx="19.5" cy="20.4" r="1.5" />
      </g>
    </svg>
  );
}
