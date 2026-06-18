import { useEffect, useState } from "react";
import AppBar from "@mui/material/AppBar";
import Toolbar from "@mui/material/Toolbar";
import Typography from "@mui/material/Typography";
import Box from "@mui/material/Box";
import Alert from "@mui/material/Alert";
import LinearProgress from "@mui/material/LinearProgress";
import { loadManifest } from "./manifest";
import { compileAndVerify, analyze } from "./toolchain";
import type { CompileResult, Analysis } from "./toolchain";
import type { Fixture, Family } from "./types";
import { FixtureList } from "./components/FixtureList";
import { DetailPane } from "./components/DetailPane";

function defaultFamily(f: Fixture): Family {
  return f.bcc ? "bcc" : "msc";
}

export function App() {
  const [fixtures, setFixtures] = useState<Fixture[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Fixture | null>(null);
  const [family, setFamily] = useState<Family>("bcc");
  const [result, setResult] = useState<CompileResult | null>(null);
  const [analysisResult, setAnalysisResult] = useState<Analysis | null>(null);
  const [busy, setBusy] = useState(false);
  const [runError, setRunError] = useState<string | null>(null);

  useEffect(() => {
    loadManifest()
      .then((m) => setFixtures(m.fixtures))
      .catch((e: unknown) => setLoadError(String(e)));
  }, []);

  // Compile + analyze whenever the selection or compiler changes.
  useEffect(() => {
    if (!selected) return;
    let cancelled = false;
    setBusy(true);
    setRunError(null);
    setResult(null);
    setAnalysisResult(null);
    (async () => {
      try {
        const r = await compileAndVerify(selected, family);
        if (cancelled) return;
        setResult(r);
        const a = await analyze(r.obj);
        if (!cancelled) setAnalysisResult(a);
      } catch (e: unknown) {
        if (!cancelled) setRunError(String(e));
      } finally {
        if (!cancelled) setBusy(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selected, family]);

  function select(f: Fixture) {
    setSelected(f);
    setFamily(defaultFamily(f));
  }

  return (
    <Box sx={{ display: "flex", flexDirection: "column", height: "100vh" }}>
      <AppBar position="static" color="default" elevation={1}>
        <Toolbar variant="dense">
          <Typography variant="h6" sx={{ flex: 1 }}>
            retrochains · corpus explorer
          </Typography>
          <Typography variant="caption" color="text.secondary">
            {fixtures ? `${fixtures.length} fixtures` : "loading…"} · compiled & verified in-browser
          </Typography>
        </Toolbar>
      </AppBar>

      {loadError && <Alert severity="error">{loadError}</Alert>}
      {!fixtures && !loadError && <LinearProgress />}

      <Box sx={{ display: "flex", flex: 1, minHeight: 0 }}>
        <Box sx={{ width: 360, borderRight: 1, borderColor: "divider", minHeight: 0 }}>
          {fixtures && (
            <FixtureList fixtures={fixtures} selectedId={selected?.id} onSelect={select} />
          )}
        </Box>
        <Box sx={{ flex: 1, minHeight: 0 }}>
          {selected ? (
            <DetailPane
              fixture={selected}
              family={family}
              onFamily={setFamily}
              result={result}
              analysis={analysisResult}
              busy={busy}
              error={runError}
            />
          ) : (
            <Box sx={{ p: 4 }}>
              <Typography color="text.secondary">
                Select a fixture to compile it in-browser, verify it against the recorded golden
                hash, and decompile it back to C.
              </Typography>
            </Box>
          )}
        </Box>
      </Box>
    </Box>
  );
}
