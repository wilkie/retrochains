int f(int x) { return x; }
int main(void) {
  int a[3];
  a[0] = 7;
  a[1] = 8;
  a[2] = 9;
  return f(a[1]);
}
