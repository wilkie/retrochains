int main(void) {
  int a[2];
  int *p;
  a[0] = 7;
  a[1] = 9;
  p = a;
  return *p++;
}
