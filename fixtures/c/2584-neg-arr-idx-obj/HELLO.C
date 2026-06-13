int a[5];
int main(void) {
  int *p;
  p = &a[2];
  return p[-1] + p[0];
}
