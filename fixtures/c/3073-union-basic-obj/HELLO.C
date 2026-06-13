union U { int i; char c; };
union U g;
int get_i(void) {
  return g.i;
}
