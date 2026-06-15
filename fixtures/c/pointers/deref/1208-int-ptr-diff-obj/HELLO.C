int main(void) {
  int a[3];
  int *p = &a[0];
  int *q = &a[2];
  return q - p;
}
