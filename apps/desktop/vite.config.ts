import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const vendorChunks: Record<string, RegExp> = {
  react: /[\\/]node_modules[\\/](react|react-dom|scheduler)[\\/]/,
  phone: /[\\/]node_modules[\\/]libphonenumber-js[\\/]/,
};

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    rollupOptions: {
      output: {
        manualChunks(id) {
          return Object.keys(vendorChunks).find((name) => vendorChunks[name].test(id));
        },
      },
    },
  },
});
