/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_OC_API?: string;
  readonly VITE_OC_COMPANY?: string;
  readonly VITE_OC_TOKEN?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
