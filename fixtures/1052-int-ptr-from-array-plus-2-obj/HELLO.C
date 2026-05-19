int main(void) {
  int a[4];
  int *p = a + 2;
  a[2] = 55;
  return *p;
}
