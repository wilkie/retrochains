int a[10];
int main(void) {
  int *p;
  int *q;
  p = &a[7];
  q = &a[2];
  return p - q;
}
