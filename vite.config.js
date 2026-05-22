import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  // Tauri expects a fixed port during dev
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Ignore the Cargo build output directory — Windows locks build
      // artifacts (.exe, .pdb) while Cargo is running, causing Vite's
      // FSWatcher to throw EBUSY and crash the dev server.
      ignored: ['**/src-tauri/target/**'],
    },
  },
  // Prevent vite from obscuring Rust errors
  clearScreen: false,
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    // Tauri uses Chromium on Windows; target modern baseline
    target: ['es2021', 'chrome100'],
    // Vite 7+ uses rolldown as the default minifier; don't force 'esbuild'
    // since it's no longer bundled with modern Vite.
    minify: !process.env.TAURI_DEBUG,
    sourcemap: !!process.env.TAURI_DEBUG,
  },
})
