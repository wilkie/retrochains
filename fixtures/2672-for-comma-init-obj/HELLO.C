int main(void) {
  int i;
  int j;
  int s;
  s = 0;
  for (i = 0, j = 10; i < 3; i = i + 1) {
    s = s + i + j;
  }
  return s;
}
