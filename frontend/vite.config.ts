import { defineConfig } from 'vite';
import solid from 'vite-plugin-solid';

export default defineConfig({
  plugins: [solid()],
  build: {
    outDir: 'dist',
    target: 'esnext',
  },
  server: {
    port: 3000,
    proxy: {
      '/api': 'http://localhost:6789',
      '/health': 'http://localhost:6789',
      '/npm-private': 'http://localhost:6789',
      '/npm-proxy': 'http://localhost:6789',
      '/npm-group': 'http://localhost:6789',
      '/cargo-private': 'http://localhost:6789',
      '/metrics': 'http://localhost:6789',
      '/-': 'http://localhost:6789',
    },
  },
});
