import adapter from "@sveltejs/adapter-static";
import { sveltekit } from "@sveltejs/kit/vite";
import { defineConfig } from "vite-plus";

export default defineConfig({
  plugins: [
    sveltekit({
      adapter: adapter({
        pages: "dist",
        assets: "dist",
        fallback: undefined,
        precompress: false,
        strict: true,
      }),
    }),
  ],
  fmt: {
    svelte: true,
    ignorePatterns: [".svelte-kit/**", "build/**", "dist/**"],
  },
  lint: {
    ignorePatterns: [".svelte-kit/**", "build/**", "dist/**"],
    options: {
      typeAware: true,
      typeCheck: true,
    },
  },
  test: {
    include: ["src/**/*.{test,spec}.{js,ts}"],
  },
});
