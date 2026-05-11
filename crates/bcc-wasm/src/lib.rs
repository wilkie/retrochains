//! WASM bindings exposed to the TypeScript `@borland-c20/bcc` package. Each
//! original tool (bcc, tlink, tasm) is reachable from a single WASM module so
//! the JS side can drive a full compile/assemble/link pipeline in-browser or in
//! Node without shelling out.
