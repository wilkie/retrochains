int dbl(int x) { return x * 2; }
int (*get_op(void))(int) {
  return dbl;
}
int main(void) {
  int (*fp)(int) = get_op();
  return fp(7);
}
