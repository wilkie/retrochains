int a[3];
int main(void) {
  int *p;
  p = a;
  *++p = 5;
  return a[1];
}
