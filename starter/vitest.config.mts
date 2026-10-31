import { fileURLToPath } from "node:url";
import { configDefaults, defineConfig } from "vitest/config";

export default defineConfig({
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  // PGlite takes several seconds to start under load; addons/ only run once copied into an app.
  test: { environment: "node", testTimeout: 30_000, exclude: [...configDefaults.exclude, "addons/**"] },
});
