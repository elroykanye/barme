/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_BARME_API?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
