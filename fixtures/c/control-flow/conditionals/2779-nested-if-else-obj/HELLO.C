int classify(int x) {
  if (x > 0) {
    if (x > 100) return 2;
    return 1;
  } else {
    return 0;
  }
}
