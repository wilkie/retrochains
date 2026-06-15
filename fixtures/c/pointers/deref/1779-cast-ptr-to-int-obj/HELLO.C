int main(void) {
  int x = 42;
  int *p = &x;
  int v = (int)p;
  return v - v + x;
}
