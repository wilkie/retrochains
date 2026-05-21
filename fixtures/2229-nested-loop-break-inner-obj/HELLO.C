int main(void) {
  int i, j;
  int s = 0;
  for (i = 0; i < 3; i++) {
    for (j = 0; j < 10; j++) {
      if (j >= 2) break;
      s += j;
    }
  }
  return s;
}
