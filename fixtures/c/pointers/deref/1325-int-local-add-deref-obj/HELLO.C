int main(void) {
  int a = 5;
  int b = 10;
  int *p = &b;
  a += *p;
  return a;
}
