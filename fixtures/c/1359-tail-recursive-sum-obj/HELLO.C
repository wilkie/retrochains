int sumto(int n, int acc) {
  if (n == 0) return acc;
  return sumto(n - 1, acc + n);
}
int main(void) {
  return sumto(5, 0);
}
