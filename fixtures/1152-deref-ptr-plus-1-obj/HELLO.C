int main(void) {
  int a[3];
  int *p = a;
  a[1] = 77;
  return *(p + 1);
}
