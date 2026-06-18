/** Pure-logic unit tests (parseBccArgs, hexdump). The wasm byte-exact verify is
 * covered by the node-based integration test (test/) and the package smoke tests. */
export default {
  preset: "ts-jest/presets/default-esm",
  testEnvironment: "node",
  extensionsToTreatAsEsm: [".ts"],
  testMatch: ["<rootDir>/src/**/*.test.ts"],
  moduleNameMapper: { "^(\\.{1,2}/.*)\\.js$": "$1" },
  transform: {
    "^.+\\.ts$": ["ts-jest", { useESM: true, isolatedModules: true }],
  },
};
