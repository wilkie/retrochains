static int helper(int x) {
  return x + 1;
}

int outer(int y) {
  return helper(y) * 2;
}
