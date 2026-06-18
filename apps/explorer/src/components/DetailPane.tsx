import { useState } from "react";
import Box from "@mui/material/Box";
import Stack from "@mui/material/Stack";
import Tabs from "@mui/material/Tabs";
import Tab from "@mui/material/Tab";
import Chip from "@mui/material/Chip";
import Typography from "@mui/material/Typography";
import Paper from "@mui/material/Paper";
import CircularProgress from "@mui/material/CircularProgress";
import ToggleButton from "@mui/material/ToggleButton";
import ToggleButtonGroup from "@mui/material/ToggleButtonGroup";
import type { Fixture, Family } from "../types";
import type { CompileResult, Analysis } from "../toolchain";
import { hexdump } from "../hexdump";
import { mono } from "../theme";

interface Props {
  fixture: Fixture;
  family: Family;
  onFamily: (f: Family) => void;
  result: CompileResult | null;
  analysis: Analysis | null;
  busy: boolean;
  error: string | null;
}

function Code({ text }: { text: string }) {
  return (
    <Paper variant="outlined" sx={{ p: 1.5, ...mono, maxHeight: "60vh" }}>
      {text}
    </Paper>
  );
}

export function DetailPane({ fixture, family, onFamily, result, analysis, busy, error }: Props) {
  const [view, setView] = useState(0);
  const families: Family[] = [
    ...(fixture.bcc ? ["bcc" as const] : []),
    ...(fixture.msc ? ["msc" as const] : []),
  ];
  const entry = fixture[family];

  return (
    <Box sx={{ p: 2, height: "100%", overflow: "auto" }}>
      <Stack direction="row" spacing={2} alignItems="center" sx={{ mb: 1 }}>
        <Typography variant="h6" sx={mono} flex={1} noWrap>
          {fixture.id}
        </Typography>
        <ToggleButtonGroup
          size="small"
          exclusive
          value={family}
          onChange={(_, v) => v && onFamily(v as Family)}
        >
          {families.map((f) => (
            <ToggleButton key={f} value={f}>
              {f}
            </ToggleButton>
          ))}
        </ToggleButtonGroup>
        {busy ? (
          <CircularProgress size={20} />
        ) : result ? (
          <Chip
            label={result.verified ? "byte-exact ✓" : "mismatch ✗"}
            color={result.verified ? "success" : "error"}
            size="small"
          />
        ) : null}
      </Stack>

      <Typography variant="caption" color="text.secondary">
        invocation: {entry?.args.join(" ")} — golden OBJ {entry?.objSha.slice(0, 12)}…
        {result ? ` — built ${result.sha.slice(0, 12)}… (${result.obj.length} B)` : ""}
      </Typography>

      {error && (
        <Paper variant="outlined" sx={{ p: 1.5, my: 1, color: "error.main", ...mono }}>
          {error}
        </Paper>
      )}

      <Typography variant="subtitle2" sx={{ mt: 2, mb: 0.5 }}>
        source — HELLO.C
      </Typography>
      <Code text={fixture.source} />

      <Tabs value={view} onChange={(_, v) => setView(v)} sx={{ mt: 2 }}>
        <Tab label="OBJ" />
        {family === "bcc" && <Tab label="ASM" />}
        <Tab label="decompiled" />
        <Tab label="analysis" />
      </Tabs>
      <Box sx={{ mt: 1 }}>
        <OutputView view={view} family={family} result={result} analysis={analysis} />
      </Box>
    </Box>
  );
}

function OutputView({
  view,
  family,
  result,
  analysis,
}: {
  view: number;
  family: Family;
  result: CompileResult | null;
  analysis: Analysis | null;
}) {
  // Tab indices shift by one when the ASM tab is absent (msc).
  const hasAsm = family === "bcc";
  const key = !hasAsm && view >= 1 ? view + 1 : view;
  if (!result) return <Typography color="text.secondary">compiling…</Typography>;

  switch (key) {
    case 0:
      return <Code text={hexdump(result.obj)} />;
    case 1:
      return <Code text={result.asm ?? "(no assembly)"} />;
    case 2:
      return <Code text={analysis?.decompiled ?? "(not fully decompilable)"} />;
    case 3:
      return analysis ? (
        <Stack spacing={1}>
          <Stack direction="row" spacing={1} alignItems="center">
            <Chip
              label={`verdict: ${analysis.classification.verdict}`}
              color="primary"
              size="small"
            />
            <Typography variant="caption" color="text.secondary">
              bcc evidence {analysis.classification.bccEvidence} · msc evidence{" "}
              {analysis.classification.mscEvidence} · {analysis.classification.idiomCount} idioms ·{" "}
              {analysis.code.length} bytes of _TEXT
            </Typography>
          </Stack>
          <Code text={hexdump(analysis.code)} />
        </Stack>
      ) : (
        <Typography color="text.secondary">no _TEXT to analyze</Typography>
      );
    default:
      return null;
  }
}
