import js from "@eslint/js";
import tseslint from "typescript-eslint";

export default tseslint.config(
  { ignores: ["dist", "wasm", "public", "playwright-report", "test-results"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    languageOptions: { parserOptions: { project: false } },
    rules: {
      // The wasm boundary returns `any`-ish JsValues; allow the non-null asserts
      // we use on manifest-validated fixture entries.
      "@typescript-eslint/no-non-null-assertion": "off",
    },
  },
);
