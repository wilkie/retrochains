int dbl(int x) {
  return x * 2;
}
int main(void) {
  int s = 0;
  int i;
  for (i = 1; i <= 3; i++) s += dbl(i);
  return s;
}
