int a[3];
int main(void) {
  int *p;
  int v;
  p = a;
  v = *++p;
  return v;
}
