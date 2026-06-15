int a[3];
int *p;
int f(int x) { return x + 1; }
int main() {
  p = a;
  a[1] = 7;
  return f(p[1]);
}
