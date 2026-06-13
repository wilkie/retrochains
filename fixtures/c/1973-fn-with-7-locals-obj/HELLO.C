int main(void) {
  int a = 1, b = 2, c = 3, d = 4, e = 5, f = 6, g = 7;
  int r = a + b + c + d + e + f + g;
  r += a * b + c * d;
  r += e * f + g;
  return r;
}
