int a[3];
int main(void) {
  int *p;
  int v;
  p = a + 2;
  v = *--p;
  return v;
}
