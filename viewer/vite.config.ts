import { fileURLToPath } from 'node:url';
import type { Connect, Plugin } from 'vite';
import { defineConfig } from 'vitest/config';

const GATEWAY = 'http://127.0.0.1:8080';

/** The gateway serves these pretty routes; mirror them in dev/preview so links work the same. */
function rewritePath(url: string): string | undefined {
  const path = url.split('?')[0] ?? url;
  if (/^\/m\/[^/]+\/?$/.test(path)) return '/viewer.html';
  switch (path) {
    case '/probe':
      return '/probe.html';
    case '/latency-target':
      return '/latency.html';
    case '/keytest':
      return '/keytest.html';
    default:
      return undefined;
  }
}

const rewrite: Connect.NextHandleFunction = (req, _res, next) => {
  const target = req.url ? rewritePath(req.url) : undefined;
  if (target) req.url = target;
  next();
};

const gatewayRoutes: Plugin = {
  name: 'rd-gateway-routes',
  configureServer: (server) => void server.middlewares.use(rewrite),
  configurePreviewServer: (server) => void server.middlewares.use(rewrite),
};

// SharedArrayBuffer (audio ring) needs cross-origin isolation, exactly like the gateway serves it.
const isolationHeaders = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Cross-Origin-Embedder-Policy': 'require-corp',
};

const proxy = {
  '/api': { target: GATEWAY },
  '/ws': { target: GATEWAY, ws: true },
};

const page = (name: string) => fileURLToPath(new URL(`./${name}.html`, import.meta.url));

export default defineConfig({
  appType: 'mpa',
  plugins: [gatewayRoutes],
  server: { host: '127.0.0.1', port: 5173, strictPort: true, headers: isolationHeaders, proxy },
  preview: { host: '127.0.0.1', port: 4173, strictPort: true, headers: isolationHeaders, proxy },
  worker: { format: 'es' },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2022',
    sourcemap: true,
    rollupOptions: {
      input: {
        index: page('index'),
        viewer: page('viewer'),
        probe: page('probe'),
        latency: page('latency'),
        keytest: page('keytest'),
      },
    },
  },
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
  },
});
