int main(void) {
  int a[3];
  int *p = &a[1];
  *p = 100;
  return a[1];
}
