int main(void) {
  int i = 0;
  int s = 0;
  while (i < 5) {
    i++;
    if (i == 2) continue;
    s += i;
  }
  return s;
}
