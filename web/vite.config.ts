import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    // Same-origin development: REST calls go through the dev proxy, so the
    // browser never sees a cross-origin request.
    proxy: {
      "/api": "http://127.0.0.1:19828",
      "/health": "http://127.0.0.1:19828",
    },
  },
})
