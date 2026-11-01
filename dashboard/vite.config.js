import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Built into dist/ and served by vineland-node, which injects the API token.
export default defineConfig({
  plugins: [react()],
  build: { outDir: "dist", emptyOutDir: true, chunkSizeWarningLimit: 4000 },
});
