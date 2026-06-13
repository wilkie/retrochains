int pow2(int n) {
  if (n == 0) return 1;
  return 2 * pow2(n - 1);
}
int main(void) {
  return pow2(6);
}
