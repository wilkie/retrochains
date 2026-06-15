int main(void) {
  int a[5];
  int *p = &a[0];
  int *q = &a[3];
  return q - p;
}
