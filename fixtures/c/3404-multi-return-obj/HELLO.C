int classify(int x) {
  if (x > 0) {
    if (x > 100) return 3;
    return 2;
  }
  if (x < 0) return 1;
  return 0;
}
