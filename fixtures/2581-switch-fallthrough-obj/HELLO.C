int main(void) {
  int x;
  int r;
  x = 1;
  r = 0;
  switch (x) {
    case 1:
      r = r + 1;
    case 2:
      r = r + 10;
      break;
    case 3:
      r = r + 100;
  }
  return r;
}
