union U {
  int i;
  char c[2];
} u;

int read_lo(void) {
  return u.c[0];
}
