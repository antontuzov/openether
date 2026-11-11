/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Base URL of the openEther core engine, e.g. `http://localhost:8080`. */
  readonly VITE_CORE_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
