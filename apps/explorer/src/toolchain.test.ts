import { parseBccArgs } from "./toolchain";

test("parses the model and lower-cases the source filename", () => {
  const o = parseBccArgs(["-c", "-ms", "HELLO.C"]);
  expect(o.model).toBe("small");
  expect(o.filename).toBe("hello.c");
});

test("parses every flag and the -D defines", () => {
  const o = parseBccArgs(["-mh", "-K", "-O", "-1", "-N", "-r-", "-d", "-DFOO=1", "-DBAR", "X.C"]);
  expect(o).toMatchObject({
    model: "huge",
    unsignedChars: true,
    optimize: true,
    target186: true,
    stackCheck: true,
    noRegVars: true,
    mergeStrings: true,
    filename: "x.c",
  });
  expect(o.defines).toEqual(["FOO=1", "BAR"]);
});

test("defaults are left unset (model/filename only when present)", () => {
  const o = parseBccArgs(["-c"]);
  expect(o.model).toBeUndefined();
  expect(o.filename).toBeUndefined();
  expect(o.defines).toBeUndefined();
});
