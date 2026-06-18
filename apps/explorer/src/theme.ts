import { createTheme } from "@mui/material/styles";

export const theme = createTheme({
  palette: {
    mode: "dark",
    primary: { main: "#7fd1b9" },
    background: { default: "#0e1116", paper: "#161b22" },
  },
  typography: {
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif",
    fontSize: 13,
  },
});

/** Monospace style shared by the code/hex panes. */
export const mono = {
  fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
  fontSize: 12,
  whiteSpace: "pre" as const,
  overflow: "auto",
};
