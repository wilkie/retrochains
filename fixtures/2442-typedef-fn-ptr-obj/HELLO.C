typedef int (*op_t)(int);
int dbl(int x) { return x + x; }
int main(void) {
  op_t f;
  f = dbl;
  return f(7);
}
