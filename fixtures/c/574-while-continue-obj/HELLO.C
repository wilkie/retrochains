int main(void) {
  int i;
  int s;
  i = 0;
  s = 0;
  while (i < 5) {
    i = i + 1;
    if (i == 3) continue;
    s = s + i;
  }
  return s;
}
