int a[3];
int *p;
int main(void) {
  p = a;
  ++p;
  *p = 99;
  return 0;
}
