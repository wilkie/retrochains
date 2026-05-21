int main(void) {
  int i, s = 0;
  for (i = 0; ; i++) {
    if (i >= 4) break;
    s += i;
  }
  return s;
}
