int b(int n);

int a(int n) {
  if (n <= 0) return 1;
  return b(n - 1);
}

int b(int n) {
  if (n <= 0) return 2;
  return a(n - 1);
}
