int main(void) {
  int x = 1;
  int r = 0;
  switch (x) {
    case 1:
    case 2:
      r = 10;
      break;
    case 3:
      r = 30;
      break;
  }
  return r;
}
