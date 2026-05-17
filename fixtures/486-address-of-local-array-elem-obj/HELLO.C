int g;
int main(void) {
  int a[3];
  int *p;
  p = &a[1];
  *p = 7;
  g = a[1];
  return 0;
}
