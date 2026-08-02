/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_BARME_API?: string;
  readonly VITE_BARME_CDN?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}

/**
 * Injected into index.html by barmed when console_api_url / console_cdn_url are
 * set. Absent when they are not, so the console falls back to deriving the URLs
 * from the address bar.
 */
interface BarmePublicUrls {
  readonly api?: string;
  readonly cdn?: string;
}

interface Window {
  readonly __BARME__?: BarmePublicUrls;
}
