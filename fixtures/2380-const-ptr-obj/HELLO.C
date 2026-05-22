int main(void) {
  int x;
  int * const p = &x;
  *p = 42;
  return x;
}
