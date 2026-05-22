int dbl(int x) { return x + x; }
int (*op)(int) = dbl;
int main(void) {
  return op(7);
}
