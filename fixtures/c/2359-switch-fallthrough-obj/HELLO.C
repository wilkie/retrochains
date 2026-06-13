int main(void) {
  int x;
  int r;
  x = 2;
  r = 0;
  switch (x) {
    case 1:
    case 2:
    case 3:
      r = 10;
      break;
    case 4:
      r = 20;
      break;
  }
  return r;
}
