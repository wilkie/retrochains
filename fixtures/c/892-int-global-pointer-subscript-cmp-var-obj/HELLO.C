int a[3];
int *p;
int main() {
  int q;
  p = a;
  a[1] = 7;
  q = 5;
  if (p[1] == q) return 1;
  return 0;
}
