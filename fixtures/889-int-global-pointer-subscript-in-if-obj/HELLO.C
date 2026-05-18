int a[3];
int *p;
int main() {
  p = a;
  a[1] = 7;
  if (p[1]) return 1;
  return 0;
}
