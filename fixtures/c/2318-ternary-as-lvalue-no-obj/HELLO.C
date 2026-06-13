int main(void) {
  int a = 0, b = 0;
  int *p = (1 ? &a : &b);
  *p = 5;
  return a + b;
}
