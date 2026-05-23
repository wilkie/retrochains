int countdown(int n) {
  if (n == 0) return 0;
  return 1 + countdown(n - 1);
}
