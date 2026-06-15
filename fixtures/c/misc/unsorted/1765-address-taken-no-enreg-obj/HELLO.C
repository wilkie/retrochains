int main(void) {
  int x = 10;
  int *p = &x;
  x = x + 1;
  x = x + 2;
  *p = *p + 3;
  return x;
}
