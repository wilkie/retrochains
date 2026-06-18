import { useMemo, useState } from "react";
import Box from "@mui/material/Box";
import List from "@mui/material/List";
import ListItemButton from "@mui/material/ListItemButton";
import ListItemText from "@mui/material/ListItemText";
import TextField from "@mui/material/TextField";
import MenuItem from "@mui/material/MenuItem";
import Stack from "@mui/material/Stack";
import Chip from "@mui/material/Chip";
import Typography from "@mui/material/Typography";
import type { Fixture } from "../types";

const RENDER_CAP = 400;

interface Props {
  fixtures: Fixture[];
  selectedId: string | undefined;
  onSelect: (f: Fixture) => void;
}

export function FixtureList({ fixtures, selectedId, onSelect }: Props) {
  const [query, setQuery] = useState("");
  const [area, setArea] = useState("all");

  const areas = useMemo(
    () => ["all", ...Array.from(new Set(fixtures.map((f) => f.area))).sort()],
    [fixtures],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return fixtures.filter(
      (f) =>
        (area === "all" || f.area === area) &&
        (q === "" || f.id.toLowerCase().includes(q) || f.source.toLowerCase().includes(q)),
    );
  }, [fixtures, query, area]);

  const shown = filtered.slice(0, RENDER_CAP);

  return (
    <Box sx={{ display: "flex", flexDirection: "column", height: "100%" }}>
      <Stack spacing={1} sx={{ p: 1 }}>
        <TextField
          size="small"
          placeholder="search id or source…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          fullWidth
        />
        <TextField
          size="small"
          select
          label="area"
          value={area}
          onChange={(e) => setArea(e.target.value)}
          fullWidth
        >
          {areas.map((a) => (
            <MenuItem key={a} value={a}>
              {a}
            </MenuItem>
          ))}
        </TextField>
        <Typography variant="caption" color="text.secondary">
          {filtered.length} match{filtered.length === 1 ? "" : "es"}
          {filtered.length > RENDER_CAP ? ` (showing first ${RENDER_CAP})` : ""}
        </Typography>
      </Stack>
      <List dense sx={{ overflow: "auto", flex: 1 }}>
        {shown.map((f) => (
          <ListItemButton key={f.id} selected={f.id === selectedId} onClick={() => onSelect(f)}>
            <ListItemText
              primary={f.name}
              secondary={f.area}
              slotProps={{ primary: { noWrap: true }, secondary: { noWrap: true } }}
            />
            <Stack direction="row" spacing={0.5}>
              {f.bcc && <Chip label="bcc" size="small" color="primary" variant="outlined" />}
              {f.msc && <Chip label="msc" size="small" variant="outlined" />}
            </Stack>
          </ListItemButton>
        ))}
      </List>
    </Box>
  );
}
