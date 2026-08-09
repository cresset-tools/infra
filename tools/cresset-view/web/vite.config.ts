import { defineConfig } from 'vite';

// The dev server serves only the frontend; every /api request belongs to the Rust backend
// (`cargo run -- --repository …`, listening on 8080). Without this proxy `npm run dev` renders
// a viewer whose every fetch 404s, and the only workable loop was a full build per change.
export default defineConfig({
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8080',
    },
  },
});
