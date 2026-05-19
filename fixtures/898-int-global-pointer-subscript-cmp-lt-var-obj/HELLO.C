int a[3];
int *p;
int g;
int main() {
  p = a;
  a[1] = 7;
  g = 10;
  if (p[1] < g) return 1;
  return 0;
}
