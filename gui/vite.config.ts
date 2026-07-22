import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Tauri 前端构建配置：dev 端口 1420 与 tauri.conf.json 对齐。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: false,
    hmr: { protocol: 'ws', host: 'localhost', port: 1421 },
    watch: { ignore: ['**/src-tauri/**'] },
  },
  build: {
    target: 'es2021',
    outDir: 'dist',
    emptyOutDir: true,
  },
});
