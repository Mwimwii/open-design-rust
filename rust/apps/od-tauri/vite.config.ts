import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// The React shell lives in `ui/`; Vite's project root points there so the
// built assets land in `ui/dist` — the directory both `tauri.conf.json`
// (frontendDist) and the embedded daemon (web_dist) serve.
export default defineConfig({
  root: 'ui',
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      // Dev-server convenience: relative /api/* reaches the daemon running
      // on its default port. In production the daemon serves this same dist
      // directory, so /api/* is same-origin with no proxy at all.
      '/api': { target: 'http://127.0.0.1:7456', changeOrigin: false },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
