char a[3];
char *p;
int f(int x) { return x + 1; }
int main() {
  p = a;
  a[1] = 7;
  return f(p[1]);
}
