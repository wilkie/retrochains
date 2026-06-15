union U { int i; char c; };
union U g;
int read_c(void) {
  return g.c;
}
