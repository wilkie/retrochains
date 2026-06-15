int inner(int a, int b, int c, int d, int e) {
  return a * 10000 + b * 1000 + c * 100 + d * 10 + e;
}
int outer(int p, int q, int r, int s, int t) {
  return inner(t, s, r, q, p);
}
int main(void) {
  return outer(1, 2, 3, 4, 5) - 54321 + 12345;
}
