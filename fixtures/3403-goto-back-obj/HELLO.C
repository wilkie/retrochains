int g;

int restart(int n) {
start:
  g++;
  n--;
  if (n > 0) goto start;
  return g;
}
