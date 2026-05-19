int main(void) {
  int a[3];
  int *p = &a[1];
  a[1] = 99;
  return *p;
}
