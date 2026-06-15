int identity(int x) { return x; }
int main(void) {
  int a = 1, b = 2, c = 3, d = 4, e = 5;
  a += b; c += d; e += a;
  identity(0);
  return a + b + c + d + e;
}
