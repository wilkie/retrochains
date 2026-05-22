int op(int a, int b);
int both(int v, int w) {
  return op(op(v, 1), op(w, 2));
}
