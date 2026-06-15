struct S { int v; };

int safe(struct S *p) {
  return p ? p->v : 0;
}
