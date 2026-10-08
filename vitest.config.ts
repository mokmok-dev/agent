import { configDefaults, defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    exclude: [...configDefaults.exclude, ".direnv/", "**/dist/**"],
    testTimeout: 20000,
  },
});
