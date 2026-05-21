typedef int (*op_t)(int);
int dbl(int x) { return x * 2; }
int main(void) {
  op_t f = dbl;
  return f(7);
}
