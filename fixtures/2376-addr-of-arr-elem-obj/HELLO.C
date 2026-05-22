int main(void) {
  int a[5];
  int *p;
  a[3] = 77;
  p = &a[3];
  return *p;
}
