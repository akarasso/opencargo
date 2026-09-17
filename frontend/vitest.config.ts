import { defineConfig } from 'vitest/config';

export default defineConfig({
  // Under node conditions solid-js resolves to its server build, where
  // createResource throws outside hydration; the stores are browser code.
  ssr: { resolve: { conditions: ['browser'], externalConditions: ['browser'] } },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
});
