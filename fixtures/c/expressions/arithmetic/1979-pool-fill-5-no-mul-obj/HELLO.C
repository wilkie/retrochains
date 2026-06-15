int main(void) {
  int a = 1, b = 2, c = 3, d = 4, e = 5;
  a += b; c += d; e += a;
  return a + b + c + d + e;
}
