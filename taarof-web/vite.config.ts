import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "");
  const proxyTarget = env.TAAROF_WEB_API_ORIGIN;

  return {
    plugins: [react()],
    server: {
      host: "127.0.0.1",
      proxy: proxyTarget
        ? {
            "/api": {
              target: proxyTarget,
              changeOrigin: true,
              ws: true,
            },
            "/health": {
              target: proxyTarget,
              changeOrigin: true,
            },
          }
        : undefined,
    },
  };
});
