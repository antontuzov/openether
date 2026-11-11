import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    // Only the TypeScript sources are tested; the compiled output in dist/ is
    // produced by the build and would otherwise be picked up as well.
    include: ['test/**/*.test.ts'],
    environment: 'node',
  },
});
