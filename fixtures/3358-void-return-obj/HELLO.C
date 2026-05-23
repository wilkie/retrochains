int g;

void early(int x) {
  if (x == 0) {
    g = -1;
    return;
  }
  g = x;
}
