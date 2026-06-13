union U { char a; char b; };
union U g;
int sz(void) {
  return sizeof(g);
}
