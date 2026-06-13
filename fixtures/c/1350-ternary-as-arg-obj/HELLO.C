int abs2(int x) {
  return x;
}
int main(void) {
  int n = -5;
  return abs2(n < 0 ? -n : n);
}
