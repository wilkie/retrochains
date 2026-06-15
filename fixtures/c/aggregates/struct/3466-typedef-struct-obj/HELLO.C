typedef struct {
  int x;
  int y;
} Pt;

Pt p;

int sum(void) {
  return p.x + p.y;
}
