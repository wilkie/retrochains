int main(void) {
  int x = 1;
  int r = 0;
  switch (x) {
    case 0: r = r + 1;
    case 1: r = r + 2;
    case 2: r = r + 3; break;
    case 3: r = r + 4; break;
  }
  return r;
}
