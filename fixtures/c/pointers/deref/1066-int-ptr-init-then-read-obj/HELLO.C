int main(void) {
  int a[4];
  int *p = a + 1;
  *p = 5;
  return *p;
}
