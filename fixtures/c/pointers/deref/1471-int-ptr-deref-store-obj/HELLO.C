int main(void) {
  int x = 5;
  int *p = &x;
  *p = 99;
  return x;
}
