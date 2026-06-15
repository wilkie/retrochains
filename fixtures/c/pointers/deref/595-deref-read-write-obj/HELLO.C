int main(void) {
  int x;
  int *p;
  x = 10;
  p = &x;
  *p = *p + 1;
  return x;
}
